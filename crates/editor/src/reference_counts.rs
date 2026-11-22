use std::{ops::Range, sync::Arc, time::Duration};

use collections::{HashMap, HashSet};
use futures::future::join_all;
use gpui::{MouseButton, SharedString, Task, TaskExt as _, WeakEntity};
use language::{BufferSnapshot, Node};
use multi_buffer::{Anchor, MultiBufferRow, MultiBufferSnapshot, ToPoint as _};
use settings::Settings as _;
use text::{BufferId, ToOffset as _};
use ui::{Context, Window, div, prelude::*};
use util::ResultExt as _;

use crate::{
    Editor, EditorSettings, FindAllReferences, SelectionEffects,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock},
};

/// Finding references is one of the most expensive language server requests,
/// so wait for typing and scrolling to settle before asking for them.
const REFERENCE_COUNTS_DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct Declaration {
    /// Start of the declaration's name, where references are requested from.
    name: text::Anchor,
    /// Start of the declaration, including any decorators: the count is shown
    /// above this line.
    start: text::Anchor,
}

struct ReferenceCountBlock {
    block_id: CustomBlockId,
    anchor: Anchor,
    name: text::Anchor,
    count: usize,
    indent_column: u32,
    /// The project generation the count was computed for. Stale counts are
    /// shown until the new ones arrive, to avoid flickering.
    generation: usize,
}

struct ReferenceCountsQuery {
    generation: usize,
    /// Name offsets of the declarations being queried.
    names: HashSet<usize>,
    _task: Task<()>,
}

pub(super) struct ReferenceCountsState {
    blocks: HashMap<BufferId, Vec<ReferenceCountBlock>>,
    queries: HashMap<BufferId, ReferenceCountsQuery>,
    /// Bumped on every edit to any buffer of the project, as an edit anywhere
    /// can add or remove usages of the functions shown here, and when language
    /// servers refresh their results.
    generation: usize,
    /// Set once invalidations settle. The next render requests the counts
    /// that went stale, so editors that aren't shown don't query anything.
    refresh_on_render: bool,
    invalidations_settled: Task<()>,
}

impl Default for ReferenceCountsState {
    fn default() -> Self {
        Self {
            blocks: HashMap::default(),
            queries: HashMap::default(),
            generation: 0,
            refresh_on_render: false,
            invalidations_settled: Task::ready(()),
        }
    }
}

impl Editor {
    pub(super) fn reference_counts_enabled_in_settings(&self, cx: &App) -> bool {
        let settings = EditorSettings::get_global(cx);
        self.enable_code_lens && settings.reference_counts && !settings.code_lens.inline()
    }

    pub(super) fn toggle_reference_counts(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled {
            self.reference_counts
                .get_or_insert_with(ReferenceCountsState::default);
            self.refresh_reference_counts(cx);
        } else {
            self.clear_reference_counts(cx);
        }
    }

    /// Requests counts for the visible declarations that don't have an
    /// up-to-date one, once scrolling or editing settles.
    pub(super) fn refresh_reference_counts(&mut self, cx: &mut Context<Self>) {
        self.request_reference_counts(REFERENCE_COUNTS_DEBOUNCE, cx);
    }

    /// Marks every count as stale: called on edits anywhere in the project, and
    /// when a language server asks for its results to be refreshed. Servers do
    /// that once they finish indexing the workspace, before which they only
    /// know about the open files and report too few references.
    pub(super) fn invalidate_reference_counts(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.reference_counts.as_mut() else {
            return;
        };
        state.generation += 1;
        state.invalidations_settled = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(REFERENCE_COUNTS_DEBOUNCE)
                .await;
            editor
                .update(cx, |editor, cx| {
                    if let Some(state) = editor.reference_counts.as_mut() {
                        state.refresh_on_render = true;
                        cx.notify();
                    }
                })
                .ok();
        });
    }

    /// Only editors that get rendered are visible, which is what keeps edits in
    /// one file from querying counts for every open tab.
    pub(super) fn refresh_stale_reference_counts_on_render(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.reference_counts.as_mut() else {
            return;
        };
        if !state.refresh_on_render {
            return;
        }
        state.refresh_on_render = false;
        cx.defer_in(window, |editor, _, cx| {
            editor.request_reference_counts(Duration::ZERO, cx);
        });
    }

    fn request_reference_counts(&mut self, debounce: Duration, cx: &mut Context<Self>) {
        if !self.lsp_data_enabled() || self.reference_counts.is_none() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };

        for (buffer_snapshot, visible_range, _) in self.visible_buffer_ranges(cx) {
            let buffer_id = buffer_snapshot.remote_id();
            let Some(buffer) = self.buffer.read(cx).buffer(buffer_id) else {
                continue;
            };
            if !self.is_lsp_relevant(buffer.read(cx).file(), cx)
                || !project
                    .read(cx)
                    .any_language_server_supports_references(buffer.read(cx), cx)
            {
                continue;
            }

            let visible_range = visible_range.start.0..visible_range.end.0;
            let declarations = declarations_in_range(&buffer_snapshot, visible_range.clone());
            self.remove_stale_reference_counts(
                buffer_id,
                &buffer_snapshot,
                &visible_range,
                &declarations,
                cx,
            );

            let Some(state) = self.reference_counts.as_mut() else {
                return;
            };
            let generation = state.generation;
            let fresh_names = state
                .blocks
                .get(&buffer_id)
                .into_iter()
                .flatten()
                .filter(|block| block.generation == generation)
                .map(|block| block.name.to_offset(&buffer_snapshot))
                .collect::<HashSet<_>>();
            let pending = declarations
                .into_iter()
                .filter(|declaration| {
                    !fresh_names.contains(&declaration.name.to_offset(&buffer_snapshot))
                })
                .collect::<Vec<_>>();
            let pending_names = pending
                .iter()
                .map(|declaration| declaration.name.to_offset(&buffer_snapshot))
                .collect::<HashSet<_>>();
            let already_queried = state.queries.get(&buffer_id).is_some_and(|query| {
                query.generation == generation && pending_names.is_subset(&query.names)
            });
            if pending.is_empty() || already_queried {
                continue;
            }

            let project = project.downgrade();
            let task = cx.spawn(async move |editor, cx| {
                if !debounce.is_zero() {
                    cx.background_executor().timer(debounce).await;
                }

                let Ok(requests) = project.update(cx, |project, cx| {
                    pending
                        .iter()
                        .map(|declaration| project.references(&buffer, declaration.name, cx))
                        .collect::<Vec<_>>()
                }) else {
                    return;
                };
                let responses = join_all(requests).await;

                editor
                    .update(cx, |editor, cx| {
                        let buffer_snapshot = buffer.read(cx).snapshot();
                        let counts = pending
                            .into_iter()
                            .zip(responses)
                            .filter_map(|(declaration, response)| {
                                let locations = response.log_err().flatten()?;
                                let name_offset = declaration.name.to_offset(&buffer_snapshot);
                                // Servers include the declaration itself, which
                                // isn't a usage.
                                let count = locations
                                    .iter()
                                    .filter(|location| {
                                        location.buffer.read(cx).remote_id() != buffer_id
                                            || location.range.start.to_offset(&buffer_snapshot)
                                                > name_offset
                                            || location.range.end.to_offset(&buffer_snapshot)
                                                < name_offset
                                    })
                                    .count();
                                Some((declaration, count))
                            })
                            .collect::<Vec<_>>();
                        editor.apply_reference_counts(buffer_id, generation, counts, cx);
                    })
                    .ok();
            });
            if let Some(state) = self.reference_counts.as_mut() {
                state.queries.insert(
                    buffer_id,
                    ReferenceCountsQuery {
                        generation,
                        names: pending_names,
                        _task: task,
                    },
                );
            }
        }
    }

    fn apply_reference_counts(
        &mut self,
        buffer_id: BufferId,
        generation: usize,
        counts: Vec<(Declaration, usize)>,
        cx: &mut Context<Self>,
    ) {
        let Some(buffer) = self.buffer.read(cx).buffer(buffer_id) else {
            return;
        };
        let buffer_snapshot = buffer.read(cx).snapshot();
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let editor_handle = cx.entity().downgrade();
        let Some(state) = self.reference_counts.as_mut() else {
            return;
        };
        let blocks = state.blocks.entry(buffer_id).or_default();

        let mut renderers_to_replace = HashMap::default();
        let mut to_insert = Vec::new();
        for (declaration, count) in counts {
            let Some(anchor) = snapshot.anchor_in_excerpt(declaration.start) else {
                continue;
            };
            let Some(name_anchor) = snapshot.anchor_in_excerpt(declaration.name) else {
                continue;
            };
            let indent_column = snapshot
                .indent_size_for_line(MultiBufferRow(anchor.to_point(&snapshot).row))
                .len;
            let name_offset = declaration.name.to_offset(&buffer_snapshot);
            let existing = blocks
                .iter_mut()
                .find(|block| block.name.to_offset(&buffer_snapshot) == name_offset);
            match existing {
                Some(block) => {
                    if block.count != count || block.indent_column != indent_column {
                        block.count = count;
                        block.indent_column = indent_column;
                        renderers_to_replace.insert(
                            block.block_id,
                            build_reference_count_renderer(
                                count,
                                indent_column,
                                name_anchor,
                                editor_handle.clone(),
                            ),
                        );
                    }
                    block.generation = generation;
                }
                None => to_insert.push((
                    BlockProperties {
                        placement: BlockPlacement::Above(anchor),
                        height: Some(1),
                        style: BlockStyle::Spacer,
                        render: build_reference_count_renderer(
                            count,
                            indent_column,
                            name_anchor,
                            editor_handle.clone(),
                        ),
                        priority: 0,
                    },
                    anchor,
                    declaration.name,
                    count,
                    indent_column,
                )),
            }
        }

        if !renderers_to_replace.is_empty() {
            self.replace_blocks(renderers_to_replace, None, cx);
        }
        if !to_insert.is_empty() {
            let mut properties = Vec::with_capacity(to_insert.len());
            let mut metadata = Vec::with_capacity(to_insert.len());
            for (block_properties, anchor, name, count, indent_column) in to_insert {
                properties.push(block_properties);
                metadata.push((anchor, name, count, indent_column));
            }
            let block_ids = self.insert_blocks(properties, None, cx);
            if let Some(state) = self.reference_counts.as_mut() {
                let blocks = state.blocks.entry(buffer_id).or_default();
                for (block_id, (anchor, name, count, indent_column)) in
                    block_ids.into_iter().zip(metadata)
                {
                    blocks.push(ReferenceCountBlock {
                        block_id,
                        anchor,
                        name,
                        count,
                        indent_column,
                        generation,
                    });
                }
            }
        }
        cx.notify();
    }

    /// Removes the counts shown in the visible range whose declaration is gone,
    /// such as after deleting or renaming a function.
    fn remove_stale_reference_counts(
        &mut self,
        buffer_id: BufferId,
        buffer_snapshot: &BufferSnapshot,
        visible_range: &Range<usize>,
        declarations: &[Declaration],
        cx: &mut Context<Self>,
    ) {
        let Some(blocks) = self
            .reference_counts
            .as_mut()
            .and_then(|state| state.blocks.get_mut(&buffer_id))
        else {
            return;
        };
        let declared_names = declarations
            .iter()
            .map(|declaration| declaration.name.to_offset(buffer_snapshot))
            .collect::<HashSet<_>>();
        let mut blocks_to_remove = HashSet::default();
        blocks.retain(|block| {
            let name_offset = block.name.to_offset(buffer_snapshot);
            let stale = !block.name.is_valid(buffer_snapshot)
                || (visible_range.contains(&name_offset) && !declared_names.contains(&name_offset));
            if stale {
                blocks_to_remove.insert(block.block_id);
            }
            !stale
        });
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }
    }

    pub(super) fn remove_invalid_reference_counts(
        &mut self,
        buffer_ids: &[BufferId],
        buffers_removed: bool,
        snapshot: &MultiBufferSnapshot,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.reference_counts.as_mut() else {
            return;
        };
        let mut blocks_to_remove = HashSet::default();
        for buffer_id in buffer_ids {
            if snapshot.buffer_for_id(*buffer_id).is_none() {
                state.queries.remove(buffer_id);
            }
            let Some(blocks) = state.blocks.get_mut(buffer_id) else {
                continue;
            };
            blocks.retain(|block| {
                let valid = !buffers_removed
                    && block.anchor.raw_text_anchor().is_none_or(|text_anchor| {
                        snapshot.anchor_in_excerpt(text_anchor).is_some()
                    });
                if !valid {
                    blocks_to_remove.insert(block.block_id);
                }
                valid
            });
            if blocks.is_empty() {
                state.blocks.remove(buffer_id);
            }
        }
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }
    }

    pub(super) fn clear_reference_counts(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.reference_counts.take() else {
            return;
        };
        let blocks_to_remove = state
            .blocks
            .into_values()
            .flatten()
            .map(|block| block.block_id)
            .collect::<HashSet<_>>();
        if !blocks_to_remove.is_empty() {
            self.remove_blocks(blocks_to_remove, None, cx);
        }
        cx.notify();
    }
}

/// Finds the named function and method declarations that intersect `range`.
///
/// Grammars name these nodes differently (`function_item`, `function_definition`,
/// `method_declaration`, ...), but they consistently mark the declared name with
/// a `name` field, which is what references are requested for.
fn declarations_in_range(buffer: &BufferSnapshot, range: Range<usize>) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    for layer in buffer.syntax_layers_for_range(range.clone(), false) {
        let mut cursor = layer.node().walk();
        'walk: loop {
            let node = cursor.node();
            if node.start_byte() < range.end && node.end_byte() > range.start {
                if let Some(declaration) = declaration_for_node(buffer, node) {
                    declarations.push(declaration);
                }
                if cursor.goto_first_child() {
                    continue;
                }
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
    }
    declarations
}

fn declaration_for_node(buffer: &BufferSnapshot, node: Node<'_>) -> Option<Declaration> {
    let kind = node.kind();
    let declares_callable =
        kind.contains("function") || kind.contains("method") || kind.contains("constructor");
    // Call sites (`method_invocation`, `call_expression`) also have a `name`
    // field in some grammars, so only accept declaration-like node kinds.
    let is_declaration = [
        "_declaration",
        "_definition",
        "_item",
        "_signature",
        "_signature_item",
    ]
    .iter()
    .any(|suffix| kind.ends_with(suffix));
    if !declares_callable || !is_declaration {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    let start = match node.parent() {
        // Keep decorators (`@property`, `@Override`) below the count.
        Some(parent) if parent.kind().contains("decorated") => parent,
        _ => node,
    };
    Some(Declaration {
        name: buffer.anchor_after(name.start_byte()),
        start: buffer.anchor_after(start.start_byte()),
    })
}

fn reference_count_label(count: usize) -> SharedString {
    match count {
        0 => SharedString::new_static("no usages"),
        1 => SharedString::new_static("1 usage"),
        count => SharedString::from(format!("{count} usages")),
    }
}

fn build_reference_count_renderer(
    count: usize,
    indent_column: u32,
    name: Anchor,
    editor: WeakEntity<Editor>,
) -> RenderBlock {
    let label = reference_count_label(count);
    Arc::new(move |cx| {
        let text_style = &cx.editor_style.text;
        let font_size = text_style.font_size.to_pixels(cx.window.rem_size()) * 0.9;
        let colors = cx.app.theme().colors();
        let color = colors
            .editor_code_lens_foreground
            .unwrap_or(colors.text_muted);

        let label = div()
            .id("reference-count")
            .font(text_style.font())
            .text_size(font_size)
            .text_color(color)
            .child(label.clone())
            .when(count > 0, |label| {
                let editor = editor.clone();
                label
                    .cursor_pointer()
                    .hover(|style| style.text_color(cx.app.theme().colors().text))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        let Some(editor) = editor.upgrade() else {
                            return;
                        };
                        editor.update(cx, |editor, cx| {
                            editor.change_selections(
                                SelectionEffects::default(),
                                window,
                                cx,
                                |selections| selections.select_anchor_ranges([name..name]),
                            );
                            if let Some(task) = editor.find_all_references(
                                &FindAllReferences {
                                    always_open_multibuffer: false,
                                    open_results_in: None,
                                },
                                window,
                                cx,
                            ) {
                                task.detach_and_log_err(cx);
                            }
                        });
                    })
            });

        div()
            .id(cx.block_id)
            .pl(cx.em_width * (indent_column as f32 + 0.5))
            .h_full()
            .flex()
            .flex_row()
            .items_end()
            .child(label)
            .into_any_element()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    };

    use gpui::{TestAppContext, UpdateGlobal as _};
    use multi_buffer::ToPoint as _;
    use settings::{CodeLens, SettingsStore};
    use text::Point;
    use util::path;

    use super::REFERENCE_COUNTS_DEBOUNCE;
    use crate::{
        Editor,
        editor_tests::{init_test, update_test_editor_settings},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    const TEXT: &str =
        "ˇfunction hello() {}\nfunction world() {}\nconst value = 1;\nhello();\nhello();";

    fn location(uri: &lsp::Uri, line: u32, start: u32, end: u32) -> lsp::Location {
        lsp::Location {
            uri: uri.clone(),
            range: lsp::Range::new(
                lsp::Position::new(line, start),
                lsp::Position::new(line, end),
            ),
        }
    }

    async fn references_test_context(cx: &mut TestAppContext) -> EditorLspTestContext {
        EditorLspTestContext::new_typescript(
            lsp::ServerCapabilities {
                references_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await
    }

    fn set_references_handler(
        cx: &mut EditorLspTestContext,
    ) -> futures::channel::mpsc::UnboundedReceiver<()> {
        cx.set_request_handler::<lsp::request::References, _, _>(|uri, params, _| async move {
            assert!(
                params.context.include_declaration,
                "the declaration is excluded from the count by the editor"
            );
            let line = params.text_document_position.position.line;
            Ok(Some(match line {
                0 => vec![
                    location(&uri, 0, 9, 14),
                    location(&uri, 3, 0, 5),
                    location(&uri, 4, 0, 5),
                ],
                1 => vec![location(&uri, 1, 9, 14)],
                _ => panic!("references requested for a non-function on line {line}"),
            }))
        })
    }

    fn reference_counts(editor: &Editor, cx: &gpui::App) -> Vec<(u32, usize)> {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut counts = editor
            .reference_counts
            .iter()
            .flat_map(|state| state.blocks.values().flatten())
            .map(|block| (block.anchor.to_point(&snapshot).row, block.count))
            .collect::<Vec<_>>();
        counts.sort();
        counts
    }

    #[gpui::test]
    async fn test_reference_counts_for_functions(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = references_test_context(cx).await;
        let mut references_requests = set_references_handler(&mut cx);

        cx.set_state(TEXT);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();

        let mut request_count = 0;
        while references_requests.try_recv().is_ok() {
            request_count += 1;
        }
        assert_eq!(request_count, 2, "only the two functions should be queried");
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 2), (1, 0)],
                "counts should exclude the declaration itself"
            );
        });

        // Servers finishing their startup work also invalidate the counts.
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        while references_requests.try_recv().is_ok() {}

        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE * 4);
        cx.run_until_parked();
        assert!(
            references_requests.try_recv().is_err(),
            "counts should not be requested again while nothing changes"
        );
    }

    #[gpui::test]
    async fn test_reference_counts_removed_with_their_function(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = references_test_context(cx).await;
        let _references_requests = set_references_handler(&mut cx);

        cx.set_state(TEXT);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(reference_counts(editor, cx), vec![(0, 2), (1, 0)]);
        });

        cx.update_editor(|editor, _, cx| {
            editor.edit([(Point::new(1, 0)..Point::new(2, 0), "")], cx);
        });
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 2)],
                "the count of a deleted function should go away right away"
            );
        });
    }

    #[gpui::test]
    async fn test_reference_counts_follow_edits_in_other_buffers(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = references_test_context(cx).await;
        let usages_of_hello = Arc::new(AtomicU32::new(2));
        cx.set_request_handler::<lsp::request::References, _, _>({
            let usages_of_hello = usages_of_hello.clone();
            move |uri, params, _| {
                let usages_of_hello = usages_of_hello.load(Ordering::SeqCst);
                async move {
                    let line = params.text_document_position.position.line;
                    let mut locations = vec![location(&uri, line, 9, 14)];
                    if line == 0 {
                        locations.extend(
                            (0..usages_of_hello).map(|usage| location(&uri, 3 + usage, 0, 5)),
                        );
                    }
                    Ok(Some(locations))
                }
            }
        });

        cx.set_state(TEXT);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(reference_counts(editor, cx), vec![(0, 2), (1, 0)]);
        });

        usages_of_hello.store(3, Ordering::SeqCst);
        let project = cx.update_editor(|editor, _, _| editor.project.clone().unwrap());
        let other_buffer = cx.update(|_, cx| {
            project.update(cx, |project, cx| {
                project.create_local_buffer("", None, false, cx)
            })
        });
        cx.update(|_, cx| {
            other_buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "hello();")], None, cx));
        });
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 3), (1, 0)],
                "an edit in another buffer should refresh the visible counts"
            );
        });
    }

    #[gpui::test]
    async fn test_reference_counts_refreshed_when_server_finishes_indexing(
        cx: &mut TestAppContext,
    ) {
        init_test(cx, |_| {});
        let mut cx = references_test_context(cx).await;
        let indexed = Arc::new(AtomicBool::new(false));
        cx.set_request_handler::<lsp::request::References, _, _>({
            let indexed = indexed.clone();
            move |uri, params, _| {
                let indexed = indexed.load(Ordering::SeqCst);
                async move {
                    let line = params.text_document_position.position.line;
                    let mut locations = vec![location(&uri, line, 9, 14)];
                    // Before indexing, the server only knows about the open file.
                    if line == 0 {
                        locations.push(location(&uri, 3, 0, 5));
                        if indexed {
                            locations.push(lsp::Location {
                                uri: lsp::Uri::from_file_path(path!("/root/other.ts")).unwrap(),
                                range: lsp::Range::new(
                                    lsp::Position::new(0, 0),
                                    lsp::Position::new(0, 5),
                                ),
                            });
                        }
                    }
                    Ok(Some(locations))
                }
            }
        });

        cx.set_state(TEXT);
        // Let the invalidations from the server's startup work settle first.
        for _ in 0..2 {
            cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
            cx.run_until_parked();
        }
        cx.editor(|editor, _, cx| {
            assert_eq!(reference_counts(editor, cx), vec![(0, 1), (1, 0)]);
        });

        indexed.store(true, Ordering::SeqCst);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE * 4);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 1), (1, 0)],
                "counts are not requested again until something invalidates them"
            );
        });

        cx.lsp
            .request::<lsp::request::CodeLensRefresh>((), lsp::DEFAULT_LSP_REQUEST_TIMEOUT)
            .await
            .into_response()
            .expect("code lens refresh request failed");
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 2), (1, 0)],
                "counts should be requested again once the server asks for a refresh"
            );
        });
    }

    #[gpui::test]
    async fn test_reference_counts_disabled(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        update_test_editor_settings(cx, &|settings| {
            settings.reference_counts = Some(false);
        });
        let mut cx = references_test_context(cx).await;
        let mut references_requests = set_references_handler(&mut cx);

        cx.set_state(TEXT);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        assert!(references_requests.try_recv().is_err());
        cx.editor(|editor, _, cx| {
            assert!(reference_counts(editor, cx).is_empty());
        });

        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.reference_counts = Some(true);
                });
            });
        });
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        cx.editor(|editor, _, cx| {
            assert_eq!(
                reference_counts(editor, cx),
                vec![(0, 2), (1, 0)],
                "enabling the setting should show counts without reopening the file"
            );
        });
    }

    #[gpui::test]
    async fn test_reference_counts_yield_to_code_lens(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        update_test_editor_settings(cx, &|settings| {
            settings.code_lens = Some(CodeLens::On);
        });
        let mut cx = references_test_context(cx).await;
        let mut references_requests = set_references_handler(&mut cx);

        cx.set_state(TEXT);
        cx.executor().advance_clock(REFERENCE_COUNTS_DEBOUNCE);
        cx.run_until_parked();
        assert!(references_requests.try_recv().is_err());
        cx.editor(|editor, _, cx| {
            assert!(reference_counts(editor, cx).is_empty());
        });
    }
}
