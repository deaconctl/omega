use std::ops::Range;

use language::Point;

/// The text of a notebook's code cells laid out as one script, in the "percent"
/// format jupytext uses: each cell follows a `# %%` marker line.
///
/// Every code cell becomes an excerpt of a single buffer holding this text, so
/// that names defined in one cell are visible to the language (and its language
/// server) in the cells after it.
pub struct CodeDocumentLayout {
    pub text: String,
    /// The lines each cell occupies, in cell order. A cell always occupies at
    /// least one line, even when it is empty, so it has somewhere to type.
    pub cell_ranges: Vec<Range<Point>>,
}

/// The source of each code cell, in notebook order.
pub fn code_cell_sources(cells: &[nbformat::v4::Cell]) -> Vec<String> {
    cells
        .iter()
        .filter_map(|cell| match cell {
            nbformat::v4::Cell::Code { source, .. } => Some(source.concat()),
            _ => None,
        })
        .collect()
}

pub fn cell_marker(comment_prefix: &str) -> String {
    format!("{} %%", comment_prefix.trim_end())
}

pub fn layout_code_cells<'a>(
    cells: impl IntoIterator<Item = &'a str>,
    comment_prefix: &str,
) -> CodeDocumentLayout {
    let marker = cell_marker(comment_prefix);
    let mut text = String::new();
    let mut cell_ranges = Vec::new();
    let mut row = 0;

    for (index, cell) in cells.into_iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(&marker);
        text.push('\n');
        row += 1;

        let start = Point::new(row, 0);
        text.push_str(cell);
        let line_count = cell.matches('\n').count() as u32;
        let last_line_len = cell.rsplit('\n').next().map_or(0, |line| line.len() as u32);
        row += line_count;
        cell_ranges.push(start..Point::new(row, last_line_len));
        row += 1;
    }

    CodeDocumentLayout { text, cell_ranges }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptCellKind {
    Code,
    Markdown,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptCell {
    pub kind: ScriptCellKind,
    pub source: String,
}

/// Writes the whole notebook as one percent-format script, the form the source
/// view edits. Markdown and raw cells are commented out, as jupytext does.
pub fn cells_to_script(cells: &[ScriptCell], comment_prefix: &str) -> String {
    let marker = cell_marker(comment_prefix);
    let comment = comment_prefix.trim_end();
    cells
        .iter()
        .map(|cell| {
            let (header, body) = match cell.kind {
                ScriptCellKind::Code => (marker.clone(), cell.source.clone()),
                ScriptCellKind::Markdown => (
                    format!("{marker} [markdown]"),
                    comment_lines(&cell.source, comment),
                ),
                ScriptCellKind::Raw => (
                    format!("{marker} [raw]"),
                    comment_lines(&cell.source, comment),
                ),
            };
            format!("{header}\n{body}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Reads a percent-format script back into cells. The blank line written
/// between cells is dropped, and text before the first marker becomes a code
/// cell.
pub fn script_to_cells(script: &str, comment_prefix: &str) -> Vec<ScriptCell> {
    let marker = cell_marker(comment_prefix);
    let comment = comment_prefix.trim_end();
    let mut cells = Vec::new();
    let mut current: Option<(ScriptCellKind, Vec<&str>)> = None;

    let finish = |current: Option<(ScriptCellKind, Vec<&str>)>,
                  followed_by_marker: bool,
                  cells: &mut Vec<ScriptCell>| {
        let Some((kind, mut lines)) = current else {
            return;
        };
        if followed_by_marker && lines.last() == Some(&"") {
            lines.pop();
        }
        let source = match kind {
            ScriptCellKind::Code => lines.join("\n"),
            ScriptCellKind::Markdown | ScriptCellKind::Raw => uncomment_lines(&lines, comment),
        };
        cells.push(ScriptCell { kind, source });
    };

    for line in script.split('\n') {
        if let Some(rest) = line.strip_prefix(marker.as_str()) {
            let rest = rest.trim();
            let kind = if rest.starts_with("[markdown]") || rest.starts_with("[md]") {
                ScriptCellKind::Markdown
            } else if rest.starts_with("[raw]") {
                ScriptCellKind::Raw
            } else {
                ScriptCellKind::Code
            };
            let previous = current.take();
            let leading_text_is_blank = cells.is_empty()
                && previous.as_ref().is_some_and(|(kind, lines)| {
                    *kind == ScriptCellKind::Code && lines.iter().all(|line| line.trim().is_empty())
                });
            if !leading_text_is_blank {
                finish(previous, true, &mut cells);
            }
            current = Some((kind, Vec::new()));
        } else {
            current
                .get_or_insert_with(|| (ScriptCellKind::Code, Vec::new()))
                .1
                .push(line);
        }
    }
    finish(current, false, &mut cells);
    cells
}

fn comment_lines(text: &str, comment: &str) -> String {
    text.split('\n')
        .map(|line| {
            if line.is_empty() {
                comment.to_string()
            } else {
                format!("{comment} {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn uncomment_lines(lines: &[&str], comment: &str) -> String {
    lines
        .iter()
        .map(|line| {
            line.strip_prefix(comment)
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(kind: ScriptCellKind, source: &str) -> ScriptCell {
        ScriptCell {
            kind,
            source: source.to_string(),
        }
    }

    #[test]
    fn test_script_round_trips_every_kind_of_cell() {
        let cells = vec![
            cell(ScriptCellKind::Markdown, "# Title\n\nSome *text*"),
            cell(ScriptCellKind::Code, "import math\n"),
            cell(ScriptCellKind::Code, ""),
            cell(ScriptCellKind::Raw, "raw text"),
            cell(ScriptCellKind::Code, "math.pi"),
        ];
        let script = cells_to_script(&cells, "# ");
        assert_eq!(
            script,
            "# %% [markdown]\n# # Title\n#\n# Some *text*\n\n# %%\nimport math\n\n\n# %%\n\n\n# %% [raw]\n# raw text\n\n# %%\nmath.pi"
        );
        assert_eq!(script_to_cells(&script, "# "), cells);
    }

    #[test]
    fn test_script_text_before_the_first_marker_is_a_code_cell() {
        assert_eq!(
            script_to_cells("x = 1\n# %%\ny = 2", "# "),
            vec![
                cell(ScriptCellKind::Code, "x = 1"),
                cell(ScriptCellKind::Code, "y = 2"),
            ]
        );
        assert_eq!(
            script_to_cells("\n# %%\ny = 2", "# "),
            vec![cell(ScriptCellKind::Code, "y = 2")]
        );
    }

    #[test]
    fn test_layout_places_each_cell_after_a_marker() {
        let layout = layout_code_cells(["import os", "", "x = 1\ny = 2"], "# ");
        assert_eq!(layout.text, "# %%\nimport os\n# %%\n\n# %%\nx = 1\ny = 2");
        assert_eq!(
            layout.cell_ranges,
            vec![
                Point::new(1, 0)..Point::new(1, 9),
                Point::new(3, 0)..Point::new(3, 0),
                Point::new(5, 0)..Point::new(6, 5),
            ]
        );
    }

    #[test]
    fn test_layout_keeps_a_trailing_newline_inside_its_cell() {
        let layout = layout_code_cells(["a\n", "b"], "#");
        assert_eq!(layout.text, "# %%\na\n\n# %%\nb");
        assert_eq!(
            layout.cell_ranges,
            vec![
                Point::new(1, 0)..Point::new(2, 0),
                Point::new(4, 0)..Point::new(4, 1),
            ]
        );
    }

    #[test]
    fn test_layout_of_no_cells_is_empty() {
        let layout = layout_code_cells(std::iter::empty(), "# ");
        assert!(layout.text.is_empty());
        assert!(layout.cell_ranges.is_empty());
    }
}
