use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use nbformat::v4::{Cell, CellId, Notebook};
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

/// Jupyter itself writes notebooks with a one-space indent.
const DEFAULT_INDENT: &str = " ";

/// The JSON a notebook was read from, kept so that saving rewrites only what was
/// edited.
///
/// `nbformat` drops fields it doesn't model and normalizes the rest (multi-line
/// strings, key order, widget output data), so serializing the parsed notebook
/// directly turns every save into a whole-file rewrite and loses data. Instead,
/// each saved value is compared with what the original JSON parses to, and the
/// original JSON is written back wherever the two agree.
pub struct NotebookFile {
    root: Map<String, Value>,
    cells: HashMap<CellId, Map<String, Value>>,
    /// Cells that had no id on disk and were given one so they can be tracked.
    /// Older notebook formats don't allow cell ids, so these aren't written out.
    synthesized_ids: HashSet<CellId>,
    indent: String,
    trailing_newline: bool,
}

impl NotebookFile {
    pub fn parse(text: &str) -> Result<(Notebook, Self)> {
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }

        let mut root: Map<String, Value> =
            serde_json::from_str(text).context("notebook is not a JSON object")?;
        let cell_values: Vec<Map<String, Value>> = match root.get("cells") {
            Some(cells) => {
                serde_json::from_value(cells.clone()).context("invalid notebook cells")?
            }
            None => Vec::new(),
        };

        let mut synthesized_ids = HashSet::default();
        let mut parse_input = root.clone();
        let cells_with_ids = cell_values
            .iter()
            .map(|cell| {
                let mut cell = cell.clone();
                if !cell.contains_key("id") {
                    let id = Uuid::new_v4().to_string();
                    cell.insert("id".into(), Value::String(id));
                }
                Value::Object(cell)
            })
            .collect();
        parse_input.insert("cells".into(), Value::Array(cells_with_ids));

        let notebook = nbformat::parse_notebook(&serde_json::to_string(&parse_input)?)
            .context("failed to parse notebook")?;
        let notebook = upgrade_notebook(notebook)?;
        if notebook.cells.len() != cell_values.len() {
            anyhow::bail!("notebook cells changed while upgrading the notebook format");
        }

        let mut cells = HashMap::default();
        for (cell, original) in notebook.cells.iter().zip(cell_values) {
            if !original.contains_key("id") {
                synthesized_ids.insert(cell.id().clone());
            }
            cells.insert(cell.id().clone(), original);
        }
        // Kept as a placeholder so the cells are written back at their
        // original position among the top-level keys.
        root.insert("cells".into(), Value::Null);

        Ok((
            notebook,
            Self {
                root,
                cells,
                synthesized_ids,
                indent: detect_indent(text),
                trailing_newline: text.ends_with('\n'),
            },
        ))
    }

    pub fn empty() -> (Notebook, Self) {
        let notebook = Notebook {
            nbformat: 4,
            nbformat_minor: 5,
            cells: Vec::new(),
            metadata: nbformat::v4::Metadata {
                kernelspec: None,
                language_info: None,
                authors: None,
                additional: HashMap::default(),
            },
        };
        let file = Self {
            root: Map::new(),
            cells: HashMap::default(),
            synthesized_ids: HashSet::default(),
            indent: DEFAULT_INDENT.into(),
            trailing_newline: true,
        };
        (notebook, file)
    }

    /// Serializes `notebook`, reusing the original JSON for everything that is
    /// unchanged, and remembers the result as the new original.
    pub fn serialize(&mut self, notebook: &Notebook) -> Result<String> {
        let new_root = match serde_json::to_value(notebook)? {
            Value::Object(root) => root,
            _ => anyhow::bail!("notebook did not serialize to a JSON object"),
        };

        let mut root = self.root.clone();
        for (key, value) in new_root {
            match key.as_str() {
                "cells" => {}
                "metadata" => {
                    let metadata = match (root.get(&key), value) {
                        (Some(Value::Object(original)), Value::Object(new)) => Value::Object(
                            merge_object::<nbformat::v4::Metadata>(original, new, |_| false),
                        ),
                        (_, value) => value,
                    };
                    root.insert(key, metadata);
                }
                // The format version is the file's own: an untouched old notebook
                // keeps its version rather than claiming a newer one.
                "nbformat" | "nbformat_minor" if root.contains_key(&key) => {}
                _ => {
                    root.insert(key, value);
                }
            }
        }

        let mut cells = HashMap::default();
        let mut cell_values = Vec::with_capacity(notebook.cells.len());
        for cell in &notebook.cells {
            let cell_value = self.merge_cell(cell)?;
            cells.insert(cell.id().clone(), cell_value.clone());
            cell_values.push(Value::Object(cell_value));
        }
        root.insert("cells".into(), Value::Array(cell_values));

        let mut output = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(self.indent.as_bytes());
        let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
        root.serialize(&mut serializer)?;
        let mut text = String::from_utf8(output)?;
        if self.trailing_newline {
            text.push('\n');
        }

        root.insert("cells".into(), Value::Null);
        self.root = root;
        self.cells = cells;
        Ok(text)
    }

    fn merge_cell(&self, cell: &Cell) -> Result<Map<String, Value>> {
        let new_cell = match serde_json::to_value(cell)? {
            Value::Object(cell) => cell,
            _ => anyhow::bail!("notebook cell did not serialize to a JSON object"),
        };
        let Some(original) = self.cells.get(cell.id()) else {
            return Ok(new_cell);
        };

        let synthesized_id = self.synthesized_ids.contains(cell.id());
        Ok(merge_object::<Cell>(original, new_cell, |key| {
            key == "id" && synthesized_id
        }))
    }
}

/// Merges `new` into `original` key by key: a key keeps its original JSON when
/// that parses (as part of a `T`) to the same value as the new one, so unchanged
/// fields are written back byte for byte and in their original order. Keys
/// `new` doesn't have are kept, and keys for which `skip` returns true are not
/// added.
fn merge_object<T>(
    original: &Map<String, Value>,
    new: Map<String, Value>,
    skip: impl Fn(&str) -> bool,
) -> Map<String, Value>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    let normalized_original = serde_json::from_value::<T>(Value::Object(original.clone()))
        .ok()
        .and_then(|value| serde_json::to_value(value).ok());
    let normalized_original = match normalized_original {
        Some(Value::Object(object)) => object,
        _ => Map::new(),
    };

    let mut merged = original.clone();
    for (key, value) in new {
        if skip(&key) {
            continue;
        }
        if normalized_original.get(&key) != Some(&value) || !merged.contains_key(&key) {
            merged.insert(key, value);
        }
    }
    merged
}

pub(super) fn upgrade_notebook(
    notebook: nbformat::Notebook,
) -> anyhow::Result<nbformat::v4::Notebook> {
    match notebook {
        nbformat::Notebook::V4(notebook) => Ok(notebook),
        nbformat::Notebook::V4QuirksMode(notebook) => {
            log::warn!("Repairing notebook cell IDs: {:?}", notebook.quirks());
            Ok(notebook.repair())
        }
        nbformat::Notebook::Legacy(notebook) => nbformat::upgrade_legacy_notebook(notebook),
        nbformat::Notebook::V3(notebook) => nbformat::upgrade_v3_notebook(notebook),
        _ => anyhow::bail!("Unsupported notebook format"),
    }
}

fn detect_indent(text: &str) -> String {
    text.lines()
        .nth(1)
        .map(|line| {
            line.chars()
                .take_while(|character| *character == ' ' || *character == '\t')
                .collect::<String>()
        })
        .filter(|indent| !indent.is_empty())
        .unwrap_or_else(|| DEFAULT_INDENT.into())
}

/// Splits cell text into the list-of-lines form notebooks store, where every
/// line but the last keeps its newline.
pub fn source_lines(text: &str) -> Vec<String> {
    text.split_inclusive('\n').map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "metadata": {},
   "source": "# Title\nSome text",
   "attachments": {
    "image.png": {
     "image/png": "iVBORw0KGgo="
    }
   }
  },
  {
   "cell_type": "code",
   "execution_count": 1,
   "metadata": {
    "tags": [
     "keep"
    ]
   },
   "outputs": [
    {
     "data": {
      "application/vnd.jupyter.widget-view+json": {
       "model_id": "abc",
       "version_major": 2,
       "version_minor": 0
      },
      "text/plain": [
       "Widget"
      ]
     },
     "metadata": {},
     "output_type": "display_data"
    }
   ],
   "source": [
    "x = 1\n",
    "x"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "custom_tool": {
   "setting": true
  }
 },
 "nbformat": 4,
 "nbformat_minor": 4
}
"##;

    #[test]
    fn test_unchanged_notebook_round_trips_exactly() -> Result<()> {
        let (notebook, mut file) = NotebookFile::parse(NOTEBOOK)?;
        assert_eq!(file.serialize(&notebook)?, NOTEBOOK);
        Ok(())
    }

    #[test]
    fn test_edited_source_changes_only_that_field() -> Result<()> {
        let (mut notebook, mut file) = NotebookFile::parse(NOTEBOOK)?;
        if let Some(Cell::Code { source, .. }) = notebook.cells.get_mut(1) {
            *source = source_lines("x = 2\nx");
        }

        let saved: Value = serde_json::from_str(&file.serialize(&notebook)?)?;
        let original: Value = serde_json::from_str(NOTEBOOK)?;
        assert_eq!(
            saved["cells"][1]["source"],
            serde_json::json!(["x = 2\n", "x"])
        );
        assert_eq!(
            saved["cells"][1]["outputs"],
            original["cells"][1]["outputs"]
        );
        assert_eq!(
            saved["cells"][1]["metadata"],
            original["cells"][1]["metadata"]
        );
        assert_eq!(saved["cells"][0], original["cells"][0]);
        assert_eq!(saved["metadata"], original["metadata"]);
        assert!(saved["cells"][1].get("id").is_none());
        Ok(())
    }

    #[test]
    fn test_new_cells_and_empty_files_serialize() -> Result<()> {
        let (mut notebook, mut file) = NotebookFile::parse("")?;
        notebook.cells.push(Cell::Code {
            id: CellId::new("new-cell").map_err(anyhow::Error::msg)?,
            metadata: serde_json::from_value(serde_json::json!({}))?,
            execution_count: None,
            source: source_lines("print(1)"),
            outputs: Vec::new(),
        });

        let text = file.serialize(&notebook)?;
        let (reparsed, _) = NotebookFile::parse(&text)?;
        assert_eq!(reparsed.cells.len(), 1);
        assert!(text.starts_with("{\n \""));
        Ok(())
    }

    #[test]
    fn test_source_lines_keep_the_last_line_bare() {
        assert_eq!(source_lines("a\nb"), vec!["a\n", "b"]);
        assert_eq!(source_lines("a\n"), vec!["a\n"]);
        assert!(source_lines("").is_empty());
    }
}
