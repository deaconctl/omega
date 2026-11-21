use std::sync::Arc;

use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{App, Context, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};

use super::NotebookEditor;

const MAX_LABEL_LENGTH: usize = 60;

/// A short name for a cell, for the outline and the breadcrumbs: a markdown
/// cell's first heading, a code cell's first definition, or else the first
/// non-blank line of either.
pub fn cell_label(source: &str, is_markdown: bool) -> String {
    let lines = || {
        source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    let label = if is_markdown {
        lines()
            .find(|line| line.starts_with('#'))
            .map(|heading| heading.trim_start_matches('#').trim())
    } else {
        lines().find(|line| {
            ["def ", "async def ", "class ", "fn ", "function "]
                .iter()
                .any(|keyword| line.starts_with(keyword))
        })
    }
    .or_else(|| lines().next())
    .unwrap_or_default();

    if label.chars().count() > MAX_LABEL_LENGTH {
        let truncated: String = label.chars().take(MAX_LABEL_LENGTH - 1).collect();
        format!("{truncated}…")
    } else {
        label.to_string()
    }
}

pub struct NotebookOutlineDelegate {
    notebook: WeakEntity<NotebookEditor>,
    /// Each cell's label, by cell index.
    entries: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl NotebookOutlineDelegate {
    pub fn new(
        notebook: WeakEntity<NotebookEditor>,
        entries: Vec<String>,
        selected_cell: usize,
    ) -> Self {
        let matches = Self::all_entries(&entries);
        Self {
            notebook,
            entries,
            matches,
            selected_index: selected_cell,
        }
    }

    fn all_entries(entries: &[String]) -> Vec<StringMatch> {
        entries
            .iter()
            .enumerate()
            .map(|(index, label)| StringMatch {
                candidate_id: index,
                score: 0.,
                positions: Vec::new(),
                string: label.clone(),
            })
            .collect()
    }
}

impl PickerDelegate for NotebookOutlineDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "notebook outline"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = index;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Go to cell…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.is_empty() {
            self.matches = Self::all_entries(&self.entries);
            self.selected_index = 0;
            cx.notify();
            return Task::ready(());
        }

        let candidates: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, label)| StringMatchCandidate::new(index, label))
            .collect();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let mut matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                100,
                &Default::default(),
                executor,
            )
            .await;
            // Notebook order reads better than score order for a list of cells.
            matches.sort_by_key(|string_match| string_match.candidate_id);
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(string_match) = self.matches.get(self.selected_index) {
            let cell_index = string_match.candidate_id;
            self.notebook
                .update(cx, |notebook, cx| {
                    notebook.reveal_cell(cell_index, window, cx)
                })
                .ok();
        }
        cx.emit(gpui::DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let string_match = self.matches.get(index)?;
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(format!("{}", string_match.candidate_id + 1))
                                .color(Color::Muted),
                        )
                        .child(HighlightedLabel::new(
                            string_match.string.clone(),
                            string_match.positions.clone(),
                        )),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_label_prefers_headings_and_definitions() {
        assert_eq!(
            cell_label("Intro\n\n## Loading data\n", true),
            "Loading data"
        );
        assert_eq!(cell_label("Some notes", true), "Some notes");
        assert_eq!(
            cell_label("import os\n\ndef load(path):\n    pass", false),
            "def load(path):"
        );
        assert_eq!(cell_label("\n  x = 1\n", false), "x = 1");
        assert_eq!(cell_label("", false), "");
    }

    #[test]
    fn test_cell_label_truncates_long_lines() {
        let label = cell_label(&"x".repeat(100), false);
        assert_eq!(label.chars().count(), MAX_LABEL_LENGTH);
        assert!(label.ends_with('…'));
    }
}
