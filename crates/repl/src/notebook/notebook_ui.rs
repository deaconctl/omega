#![allow(unused, dead_code)]
use std::future::Future;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use buffer_diff::{BufferDiff, BufferDiffEvent};
use client::proto::ViewId;
use collections::{HashMap, HashSet};
use editor::DisplayPoint;
use feature_flags::{FeatureFlagAppExt as _, NotebookFeatureFlag};
use futures::FutureExt;
use futures::future::Shared;
use gpui::{
    AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, Font, Global, KeyContext,
    ListScrollEvent, ListState, Point, Subscription, Task, TaskExt, WeakEntity, actions, list,
    prelude::*,
};
use jupyter_protocol::JupyterKernelspec;
use language::{Buffer, HighlightedText, Language, LanguageRegistry};
use log;
use project::search::SearchQuery;
use project::{Project, ProjectEntryId, ProjectPath};
use settings::SeedQuerySetting;
use settings::{Settings as _, SettingsStore};
use ui::{CommonAnimationExt, KeyBinding, Tooltip, prelude::*};
use workspace::item::{ItemEvent, SaveOptions, TabContentParams};
use workspace::searchable::{
    Direction, SearchEvent, SearchOptions, SearchToken, SearchableItem, SearchableItemHandle,
};
use workspace::{Item, ItemHandle, Pane, ProjectItem, ToolbarItemLocation, Workspace};

use super::code_document::{
    ScriptCell, ScriptCellKind, cells_to_script, code_cell_sources, layout_code_cells,
    script_to_cells,
};
use super::file_format::NotebookFile;
use super::{Cell, CellEvent, CellPosition, MarkdownCellEvent, RenderableCell};

use nbformat::v4::CellId;
use nbformat::v4::Metadata as NotebookMetadata;
use serde_json;
use util::ResultExt as _;
use uuid::Uuid;

use crate::JupyterSettings;
use crate::components::{KernelPickerDelegate, KernelSelector};
use crate::kernels::{
    Kernel, KernelSession, KernelSpecification, KernelStatus, LocalKernelSpecification,
    NativeRunningKernel, RemoteRunningKernel, SshRunningKernel, WslRunningKernel,
};
use crate::notebook::MovementDirection;
use crate::repl_store::ReplStore;

use jupyter_protocol::{ExecuteRequest, JupyterMessage, JupyterMessageContent};
use picker::Picker;
use ui::PopoverMenuHandle;
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::notebook::{
    AddCodeBlock, AddMarkdownBlock, ClearOutputs, DeleteCell, EnterCommandMode, EnterEditMode,
    InterruptKernel, MoveCellDown, MoveCellUp, NotebookMoveDown, NotebookMoveUp, OpenNotebook,
    RestartKernel, Run, RunAll, RunAndAdvance, ToggleSourceView,
};

/// Whether the notebook is in command mode (navigating cells) or edit mode (editing a cell).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotebookMode {
    Command,
    Edit,
}

#[derive(PartialEq, Eq)]
enum SelectionMode {
    SelectOnly,
    SelectAndMove,
}

pub(crate) const MAX_TEXT_BLOCK_WIDTH: f32 = 9999.0;
pub(crate) const SMALL_SPACING_SIZE: f32 = 8.0;
pub(crate) const MEDIUM_SPACING_SIZE: f32 = 12.0;
pub(crate) const LARGE_SPACING_SIZE: f32 = 16.0;
pub(crate) const GUTTER_WIDTH: f32 = 19.0;
pub(crate) const CODE_BLOCK_INSET: f32 = MEDIUM_SPACING_SIZE;
pub(crate) const CONTROL_SIZE: f32 = 20.0;

const NOTEBOOK_EXTENSION: &str = "ipynb";
/// Used for the cell markers in the shared code buffer until the notebook's
/// language has loaded. Most kernel languages comment with `#`.
const DEFAULT_COMMENT_PREFIX: &str = "# ";

/// Marks the notebook editor as registered. Registration can be triggered by the
/// feature flag and by the setting, and registering twice would add a second
/// opener for `.ipynb` files.
struct NotebookEditorRegistered;

impl Global for NotebookEditorRegistered {}

fn register_notebook_editor(cx: &mut App) {
    if cx.has_global::<NotebookEditorRegistered>() {
        return;
    }
    workspace::register_project_item::<NotebookEditor>(cx);
    cx.set_global(NotebookEditorRegistered);
}

/// The sidecar files currently backing an open notebook editor.
#[derive(Default)]
struct NotebookSidecars(HashSet<PathBuf>);

impl Global for NotebookSidecars {}

/// The hidden file next to `notebook_path` that backs its code cells, such as
/// `.analysis.ipynb.py` for `analysis.ipynb`.
fn sidecar_path(notebook_path: &Path, extension: &str) -> Option<PathBuf> {
    let file_name = notebook_path.file_name()?.to_str()?;
    Some(notebook_path.with_file_name(format!(".{file_name}.{extension}")))
}

/// For each cell of `new`, the index of the cell of `old` it continues, if any.
///
/// Cells that are unchanged are matched first, as the longest common
/// subsequence of the two lists. Between those, edited cells are matched by
/// position when they are of the same kind, so that editing a cell's text in the
/// source view keeps its id and outputs. Remaining cells reuse an unmatched old
/// cell with identical text, which covers cells that were moved.
fn align_cells(old: &[ScriptCell], new: &[ScriptCell]) -> Vec<Option<usize>> {
    let mut lengths = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            lengths[old_index][new_index] = if old[old_index] == new[new_index] {
                lengths[old_index + 1][new_index + 1] + 1
            } else {
                lengths[old_index + 1][new_index].max(lengths[old_index][new_index + 1])
            };
        }
    }

    let mut matches = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() && new_index < new.len() {
        if old[old_index] == new[new_index] {
            matches.push((old_index, new_index));
            old_index += 1;
            new_index += 1;
        } else if lengths[old_index + 1][new_index] >= lengths[old_index][new_index + 1] {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }

    let mut assignment = vec![None; new.len()];
    let (mut old_start, mut new_start) = (0, 0);
    for (old_end, new_end) in matches
        .iter()
        .copied()
        .chain(std::iter::once((old.len(), new.len())))
    {
        for (old_index, new_index) in (old_start..old_end).zip(new_start..new_end) {
            if old[old_index].kind == new[new_index].kind {
                assignment[new_index] = Some(old_index);
            }
        }
        if new_end < new.len() {
            assignment[new_end] = Some(old_end);
        }
        old_start = old_end + 1;
        new_start = new_end + 1;
    }

    // A cell moved past others isn't part of the common subsequence; it is still
    // the same cell wherever its unchanged text turns up.
    let mut used = vec![false; old.len()];
    for old_index in assignment.iter().flatten() {
        used[*old_index] = true;
    }
    for (new_index, slot) in assignment.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        if let Some(old_index) =
            (0..old.len()).find(|old_index| !used[*old_index] && old[*old_index] == new[new_index])
        {
            used[old_index] = true;
            *slot = Some(old_index);
        }
    }
    assignment
}

fn new_nbformat_cell(id: CellId, cell: &ScriptCell) -> nbformat::v4::Cell {
    let metadata = nbformat::v4::CellMetadata::default();
    let source = super::file_format::source_lines(&cell.source);
    match cell.kind {
        ScriptCellKind::Code => nbformat::v4::Cell::Code {
            id,
            metadata,
            execution_count: None,
            source,
            outputs: Vec::new(),
        },
        ScriptCellKind::Markdown => nbformat::v4::Cell::Markdown {
            id,
            metadata,
            source,
            attachments: None,
        },
        ScriptCellKind::Raw => nbformat::v4::Cell::Raw {
            id,
            metadata,
            source,
        },
    }
}

fn notebook_editor_enabled(cx: &App) -> bool {
    JupyterSettings::get_global(cx).notebook_enabled
        || cx.has_flag::<NotebookFeatureFlag>()
        || std::env::var("LOCAL_NOTEBOOK_DEV").is_ok()
}

pub fn init(cx: &mut App) {
    if notebook_editor_enabled(cx) {
        register_notebook_editor(cx);
    }

    // There is no way to unregister a project item, so turning the notebook editor
    // off again only takes effect after a restart.
    cx.observe_flag::<NotebookFeatureFlag, _>(|flag, cx| {
        if *flag {
            register_notebook_editor(cx);
        }
    })
    .detach();
    cx.observe_global::<SettingsStore>(|cx| {
        if JupyterSettings::get_global(cx).notebook_enabled {
            register_notebook_editor(cx);
        }
    })
    .detach();
}

pub struct NotebookEditor {
    languages: Arc<LanguageRegistry>,
    project: Entity<Project>,
    worktree_id: project::WorktreeId,
    focus_handle: FocusHandle,
    notebook_item: Entity<NotebookItem>,
    notebook_language: Shared<Task<Option<Arc<Language>>>>,
    remote_id: Option<ViewId>,
    cell_list: ListState,
    notebook_mode: NotebookMode,
    selected_cell_index: usize,
    cell_order: Vec<CellId>,
    original_cell_order: Vec<CellId>,
    cell_map: HashMap<CellId, Cell>,
    /// Every code cell's text, as one buffer that each cell editor shows an
    /// excerpt of.
    code_document: Entity<Buffer>,
    comment_prefix: String,
    _code_document_reparse: Task<()>,
    /// The hidden file backing `code_document` for language servers, when
    /// `jupyter.language_server_sidecar` is on.
    sidecar_path: Option<PathBuf>,
    _sidecar_task: Task<()>,
    /// The whole notebook as one percent-format script, while the source view
    /// is shown instead of the cells.
    source_view: Option<SourceView>,
    workspace: Option<WeakEntity<Workspace>>,
    nav_history: Option<workspace::ItemNavHistory>,
    /// The shared code buffer's changes against the committed notebook, drawn in
    /// each code cell's gutter.
    code_diff: Option<Entity<BufferDiff>>,
    /// The `.ipynb` file's own diff against HEAD, kept for its base text.
    committed_diff: Option<Entity<BufferDiff>>,
    /// The committed notebook's code cells, laid out like the shared code buffer.
    committed_projection: Option<Arc<str>>,
    /// The committed notebook's markdown cells, in order.
    committed_markdown: Vec<(CellId, String)>,
    /// Each markdown cell's diff against its committed version, with the task
    /// computing it.
    markdown_diffs: HashMap<CellId, (Entity<BufferDiff>, Task<()>)>,
    _committed_diff_task: Task<()>,
    _code_diff_update: Task<()>,
    _code_document_subscription: Subscription,
    _committed_diff_subscription: Option<Subscription>,
    kernel: Kernel,
    kernel_specification: Option<KernelSpecification>,
    execution_requests: HashMap<String, CellId>,
    /// Cells run while the kernel was still starting, to run once it is ready.
    queued_executions: Vec<CellId>,
    kernel_picker_handle: PopoverMenuHandle<Picker<KernelPickerDelegate>>,
}

struct SourceView {
    editor: Entity<editor::Editor>,
    /// The script as it was last written from the cells, to tell whether the
    /// source view has edits the cells don't have yet.
    applied_script: String,
}

enum SaveDestination {
    CurrentPath,
    NewPath(ProjectPath),
}

impl NotebookEditor {
    pub fn new(
        project: Entity<Project>,
        notebook_item: Entity<NotebookItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let languages = project.read(cx).languages().clone();
        let language_name = notebook_item.read(cx).language_name();
        let worktree_id = notebook_item.read(cx).project_path.worktree_id;

        let notebook_language = notebook_item.read(cx).notebook_language();
        let notebook_language = cx
            .spawn_in(window, async move |_, _| notebook_language.await)
            .shared();

        let comment_prefix = DEFAULT_COMMENT_PREFIX.to_string();
        let notebook = notebook_item.read(cx).notebook.clone();
        let sources = code_cell_sources(&notebook.cells);
        let layout = layout_code_cells(sources.iter().map(String::as_str), &comment_prefix);
        let (code_document, code_document_reparse) =
            super::new_cell_buffer(&layout.text, languages.clone(), cx);
        let (cell_order, cell_map) = Self::load_cells(
            &notebook.cells,
            &languages,
            &code_document,
            layout.cell_ranges,
            &project,
            window,
            cx,
        );

        let notebook_handle = cx.entity().downgrade();
        let cell_count = cell_order.len();

        let this = cx.entity();
        let cell_list = ListState::new(cell_count, gpui::ListAlignment::Top, px(1000.));

        let mut editor = Self {
            project,
            languages: languages.clone(),
            worktree_id,
            focus_handle,
            notebook_item: notebook_item.clone(),
            notebook_language,
            remote_id: None,
            cell_list,
            notebook_mode: NotebookMode::Command,
            selected_cell_index: 0,
            cell_order: cell_order.clone(),
            original_cell_order: cell_order.clone(),
            cell_map: cell_map.clone(),
            code_document,
            comment_prefix,
            _code_document_reparse: code_document_reparse,
            sidecar_path: None,
            _sidecar_task: Task::ready(()),
            source_view: None,
            workspace: None,
            nav_history: None,
            code_diff: None,
            committed_diff: None,
            committed_projection: None,
            committed_markdown: Vec::new(),
            markdown_diffs: HashMap::default(),
            _committed_diff_task: Task::ready(()),
            _code_diff_update: Task::ready(()),
            _code_document_subscription: Subscription::new(|| {}),
            _committed_diff_subscription: None,
            kernel: Kernel::Shutdown,
            kernel_specification: None,
            execution_requests: HashMap::default(),
            queued_executions: Vec::new(),
            kernel_picker_handle: PopoverMenuHandle::default(),
        };
        for cell_id in cell_order {
            if let Some(cell) = cell_map.get(&cell_id) {
                editor.subscribe_to_cell(cell_id, cell, window, cx);
            }
        }
        editor.discover_kernels_and_launch(window, cx);
        editor.refresh_language(cx);
        editor.start_sidecar(cx);
        editor.observe_code_document(cx);
        editor.start_diff(cx);
        cx.on_release(|this, cx| this.remove_sidecar(cx)).detach();

        cx.subscribe(&notebook_item, |this, _item, _event, cx| {
            this.refresh_language(cx);
        })
        .detach();

        editor
    }

    /// Launches the kernel once the project's Python environments are known.
    /// Choosing earlier would pick a bare `python3` over the project's own
    /// environment, since that isn't discovered yet.
    fn discover_kernels_and_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let worktree_id = self.worktree_id;
        let discovery = ReplStore::global(cx).update(cx, |store, cx| {
            store.refresh_python_kernelspecs(worktree_id, &project, cx)
        });

        let launch = cx
            .spawn_in(window, async move |this, cx| {
                discovery
                    .await
                    .context("failed to discover Python environments")
                    .log_err();
                this.update_in(cx, |this, window, cx| {
                    if matches!(this.kernel, Kernel::StartingKernel(_)) {
                        this.launch_kernel(window, cx);
                    }
                })
                .ok();
            })
            .shared();
        self.kernel = Kernel::StartingKernel(launch);
    }

    /// The kernel to start when the user hasn't picked one for this notebook.
    fn preferred_kernelspec(&self, cx: &App) -> Option<KernelSpecification> {
        let store = ReplStore::global(cx).read(cx);
        if let Some(selected) = store.selected_kernel(self.worktree_id) {
            return Some(selected.clone());
        }

        let notebook_item = self.notebook_item.read(cx);
        let language = notebook_item.language_name();
        let matches_language = |spec: &KernelSpecification| {
            language
                .as_ref()
                .is_none_or(|language| spec.language().eq_ignore_ascii_case(language))
        };
        let specs: Vec<_> = store
            .kernel_specifications_for_worktree(self.worktree_id)
            .filter(|spec| matches_language(spec))
            .cloned()
            .collect();

        // A kernel chosen before, recorded by interpreter path because several
        // environments often share a name such as "Python 3".
        let saved_choice = language.as_ref().and_then(|language| {
            JupyterSettings::get_global(cx)
                .kernel_selections
                .get(language)
        });
        if let Some(saved_choice) = saved_choice
            && let Some(spec) = specs.iter().find(|spec| {
                spec.path().as_ref() == saved_choice.as_str()
                    || spec.name().eq_ignore_ascii_case(saved_choice)
            })
        {
            return Some(spec.clone());
        }

        let worktree_path = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path());
        let project_environment = specs.iter().find(|spec| {
            matches!(spec, KernelSpecification::PythonEnv(_))
                && spec.has_ipykernel()
                && worktree_path.as_ref().is_some_and(|worktree_path| {
                    Path::new(spec.path().as_ref()).starts_with(worktree_path)
                })
        });
        let active_toolchain = || {
            let active_path = store.active_python_toolchain_path(self.worktree_id)?;
            specs
                .iter()
                .find(|spec| spec.has_ipykernel() && spec.path() == *active_path)
        };
        let named_in_notebook = || {
            let name = notebook_item
                .notebook
                .metadata
                .kernelspec
                .as_ref()?
                .name
                .clone();
            specs.iter().find(|spec| {
                matches!(spec, KernelSpecification::Jupyter(_)) && spec.name() == name.as_str()
            })
        };
        project_environment
            .or_else(active_toolchain)
            .or_else(named_in_notebook)
            .or_else(|| specs.iter().find(|spec| spec.has_ipykernel()))
            .cloned()
    }

    /// Records `spec` in the notebook's metadata and as the default for its
    /// language, after the user picks it.
    fn remember_kernel_choice(&mut self, spec: &KernelSpecification, cx: &mut Context<Self>) {
        let display_name = match spec {
            KernelSpecification::Jupyter(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::PythonEnv(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::JupyterServer(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::SshRemote(s) => s.kernelspec.display_name.clone(),
            KernelSpecification::WslRemote(s) => s.kernelspec.display_name.clone(),
        };
        let kernelspec_json = serde_json::json!({
            "display_name": display_name,
            "name": spec.name().to_string(),
            "language": spec.language().to_string(),
        });
        self.notebook_item.update(cx, |item, cx| {
            if let Ok(kernelspec) = serde_json::from_value(kernelspec_json) {
                item.notebook.metadata.kernelspec = Some(kernelspec);
                cx.emit(());
            }
        });

        ReplStore::global(cx).update(cx, |store, cx| {
            store.set_active_kernelspec(self.worktree_id, spec.clone(), cx)
        });
        if let Some(language) = self.notebook_item.read(cx).language_name() {
            let path = spec.path().to_string();
            settings::update_settings_file(
                self.project.read(cx).fs().clone(),
                cx,
                move |settings, _| {
                    settings
                        .editor
                        .jupyter
                        .get_or_insert_default()
                        .kernel_selections
                        .get_or_insert_default()
                        .insert(language, path);
                },
            );
        }
    }

    fn run_queued_executions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell_id in std::mem::take(&mut self.queued_executions) {
            self.execute_cell(cell_id, window, cx);
        }
    }

    fn fail_queued_executions(&mut self, error: &str, window: &mut Window, cx: &mut Context<Self>) {
        for cell_id in std::mem::take(&mut self.queued_executions) {
            if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                cell.update(cx, |cell, cx| {
                    cell.show_kernel_error(
                        &format!("the kernel failed to launch: {error}"),
                        window,
                        cx,
                    )
                });
            }
        }
    }

    fn refresh_language(&mut self, cx: &mut Context<Self>) {
        let notebook_language = self.notebook_item.read(cx).notebook_language();
        let task = cx.spawn(async move |this, cx| {
            let language = notebook_language.await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    let comment_prefix = language
                        .as_ref()
                        .and_then(|language| language.config().line_comments.first().cloned())
                        .map_or_else(
                            || DEFAULT_COMMENT_PREFIX.to_string(),
                            |prefix| prefix.to_string(),
                        );
                    this.code_document.update(cx, |document, cx| {
                        document.set_language(language.clone(), cx);
                    });
                    if comment_prefix != this.comment_prefix {
                        this.comment_prefix = comment_prefix;
                        this.rebuild_code_document(cx);
                        this.refresh_committed_projection(cx);
                    }
                });
            }
            language
        });
        self.notebook_language = task.shared();
    }

    fn load_cells(
        cells: &[nbformat::v4::Cell],
        languages: &Arc<LanguageRegistry>,
        code_document: &Entity<Buffer>,
        code_ranges: Vec<std::ops::Range<language::Point>>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut App,
    ) -> (Vec<CellId>, HashMap<CellId, Cell>) {
        let mut code_ranges = code_ranges.into_iter();
        let mut cell_order = Vec::with_capacity(cells.len());
        let mut cell_map = HashMap::default();
        for cell in cells {
            let code_range = match cell {
                nbformat::v4::Cell::Code { .. } => code_ranges.next(),
                _ => None,
            };
            let cell_entity = Cell::load(
                cell,
                languages,
                code_document,
                code_range,
                project,
                window,
                cx,
            );
            cell_order.push(cell.id().clone());
            cell_map.insert(cell.id().clone(), cell_entity);
        }
        (cell_order, cell_map)
    }

    /// Moves the shared code buffer onto a hidden file next to the notebook, since
    /// language servers only attach to buffers backed by a file.
    fn start_sidecar(&mut self, cx: &mut Context<Self>) {
        let project = self.project.read(cx);
        if !JupyterSettings::get_global(cx).language_server_sidecar || !project.is_local() {
            return;
        }
        let Some(notebook_path) =
            project.absolute_path(&self.notebook_item.read(cx).project_path, cx)
        else {
            return;
        };
        let fs = project.fs().clone();
        let project = self.project.clone();
        let notebook_language = self.notebook_language.clone();

        self._sidecar_task = cx.spawn(async move |this, cx| {
            let result = async {
                let Some(language) = notebook_language.await else {
                    return anyhow::Ok(());
                };
                let Some(path) = language
                    .path_suffixes()
                    .first()
                    .and_then(|extension| sidecar_path(&notebook_path, extension))
                else {
                    return Ok(());
                };
                // A second editor for the same notebook, such as a split, keeps an
                // in-memory buffer: two editors rewriting one file would clobber
                // each other's cells.
                let claimed = cx.update(|cx| {
                    cx.default_global::<NotebookSidecars>()
                        .0
                        .insert(path.clone())
                });
                if !claimed {
                    return Ok(());
                }
                let text = this.update(cx, |this, cx| {
                    this.sidecar_path = Some(path.clone());
                    this.code_document.read(cx).text()
                })?;

                fs.atomic_write(path.clone(), text).await?;
                let buffer = project
                    .update(cx, |project, cx| project.open_local_buffer(&path, cx))
                    .await?;
                this.update(cx, |this, cx| {
                    this.code_document = buffer;
                    this.observe_code_document(cx);
                    this.rebuild_code_document(cx);
                })
            }
            .await;
            result
                .context("failed to create the notebook's language server file")
                .log_err();
        });
    }

    fn observe_code_document(&mut self, cx: &mut Context<Self>) {
        self._code_document_subscription =
            cx.subscribe(&self.code_document, |this, _, event, cx| {
                if let language::BufferEvent::Edited { .. } = event {
                    this.update_code_diff(cx);
                    cx.emit(SearchEvent::MatchesInvalidated);
                    cx.emit(ItemEvent::UpdateBreadcrumbs);
                }
            });
    }

    fn start_diff(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let project_path = self.notebook_item.read(cx).project_path.clone();
        self._committed_diff_task = cx.spawn(async move |this, cx| {
            let result = async {
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(project_path, cx))
                    .await?;
                let committed_diff = project
                    .update(cx, |project, cx| project.open_uncommitted_diff(buffer, cx))
                    .await?;
                this.update(cx, |this, cx| {
                    this._committed_diff_subscription =
                        Some(cx.subscribe(&committed_diff, |this, _, event, cx| {
                            if let BufferDiffEvent::BaseTextChanged = event {
                                this.refresh_committed_projection(cx);
                            }
                        }));
                    this.committed_diff = Some(committed_diff);
                    this.refresh_committed_projection(cx);
                })
            }
            .await;
            // Notebooks outside a git repository have nothing to diff against.
            result.log_err();
        });
    }

    fn refresh_committed_projection(&mut self, cx: &mut Context<Self>) {
        let committed_text = self
            .committed_diff
            .as_ref()
            .and_then(|diff| diff.read(cx).base_text_string(cx));
        let committed_notebook = committed_text.and_then(|text| {
            NotebookFile::parse(&text)
                .context("failed to parse the committed notebook")
                .log_err()
                .map(|(notebook, _)| notebook)
        });
        self.committed_projection = committed_notebook.as_ref().map(|notebook| {
            let sources = code_cell_sources(&notebook.cells);
            let layout =
                layout_code_cells(sources.iter().map(String::as_str), &self.comment_prefix);
            Arc::from(layout.text)
        });
        self.committed_markdown = committed_notebook
            .map(|notebook| {
                notebook
                    .cells
                    .into_iter()
                    .filter_map(|cell| match cell {
                        nbformat::v4::Cell::Markdown { id, source, .. } => {
                            Some((id, source.concat()))
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.update_code_diff(cx);
        self.update_markdown_diffs(cx);
    }

    /// Diffs each markdown cell against the committed cell with the same id, or
    /// else the committed markdown cell at the same position. Markdown cells
    /// aren't part of the shared code buffer, so they get a diff of their own.
    fn update_markdown_diffs(&mut self, cx: &mut Context<Self>) {
        if self.committed_projection.is_none() {
            return;
        }
        let markdown_cells: Vec<_> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| match self.cell_map.get(cell_id) {
                Some(Cell::Markdown(markdown_cell)) => {
                    Some((cell_id.clone(), markdown_cell.clone()))
                }
                _ => None,
            })
            .collect();
        self.markdown_diffs
            .retain(|cell_id, _| markdown_cells.iter().any(|(id, _)| id == cell_id));

        for (position, (cell_id, markdown_cell)) in markdown_cells.into_iter().enumerate() {
            let Some(buffer) = markdown_cell.read(cx).buffer(cx) else {
                continue;
            };
            let base_text: Arc<str> = self
                .committed_markdown
                .iter()
                .find(|(committed_id, _)| *committed_id == cell_id)
                .or_else(|| self.committed_markdown.get(position))
                .map_or_else(|| Arc::from(""), |(_, source)| Arc::from(source.as_str()));
            let snapshot = buffer.read(cx).text_snapshot();
            let diff = match self.markdown_diffs.get(&cell_id) {
                Some((diff, _)) if diff.read(cx).buffer_id == snapshot.remote_id() => diff.clone(),
                _ => {
                    let language = buffer.read(cx).language().cloned();
                    let languages = self.languages.clone();
                    let diff =
                        cx.new(|cx| BufferDiff::new(&snapshot, language, Some(languages), cx));
                    markdown_cell.update(cx, |markdown_cell, cx| {
                        markdown_cell.set_diff(diff.clone(), cx)
                    });
                    diff
                }
            };
            let task = diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(base_text), snapshot, cx)
            });
            self.markdown_diffs.insert(cell_id, (diff, task));
        }
    }

    fn update_code_diff(&mut self, cx: &mut Context<Self>) {
        let Some(base_text) = self.committed_projection.clone() else {
            return;
        };
        let snapshot = self.code_document.read(cx).text_snapshot();
        let diff = match &self.code_diff {
            Some(diff) if diff.read(cx).buffer_id == snapshot.remote_id() => diff.clone(),
            _ => {
                let language = self.code_document.read(cx).language().cloned();
                let languages = self.languages.clone();
                let diff = cx.new(|cx| BufferDiff::new(&snapshot, language, Some(languages), cx));
                self.code_diff = Some(diff.clone());
                diff
            }
        };
        for code_cell in self.code_cells_in_order() {
            code_cell.update(cx, |code_cell, cx| code_cell.set_diff(diff.clone(), cx));
        }
        // Replacing the task cancels a diff still being computed for older text.
        self._code_diff_update = diff.update(cx, |diff, cx| {
            diff.set_base_text(Some(base_text), snapshot, cx)
        });
    }

    fn remove_sidecar(&mut self, cx: &mut App) {
        let Some(path) = self.sidecar_path.take() else {
            return;
        };
        cx.default_global::<NotebookSidecars>().0.remove(&path);
        let fs = self.project.read(cx).fs().clone();
        cx.background_spawn(async move {
            fs.remove_file(
                &path,
                fs::RemoveOptions {
                    recursive: false,
                    ignore_if_not_exists: true,
                },
            )
            .await
        })
        .detach_and_log_err(cx);
    }

    /// Replaces every cell with `cells`, as when the file is reloaded from disk.
    fn replace_cells(
        &mut self,
        cells: &[nbformat::v4::Cell],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sources = code_cell_sources(cells);
        let layout = layout_code_cells(sources.iter().map(String::as_str), &self.comment_prefix);
        self.code_document.update(cx, |document, cx| {
            document.set_text(layout.text, cx);
        });
        let (cell_order, cell_map) = Self::load_cells(
            cells,
            &self.languages,
            &self.code_document,
            layout.cell_ranges,
            &self.project,
            window,
            cx,
        );
        for cell_id in &cell_order {
            if let Some(cell) = cell_map.get(cell_id) {
                self.subscribe_to_cell(cell_id.clone(), cell, window, cx);
            }
        }

        self.cell_order = cell_order.clone();
        self.original_cell_order = cell_order;
        self.cell_map = cell_map;
        self.selected_cell_index = self
            .selected_cell_index
            .min(self.cell_order.len().saturating_sub(1));
        self.cell_list = ListState::new(self.cell_order.len(), gpui::ListAlignment::Top, px(1000.));
        self.update_markdown_diffs(cx);
        cx.notify();
    }

    fn subscribe_to_cell(
        &mut self,
        cell_id: CellId,
        cell: &Cell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match cell {
            Cell::Code(code_cell) => {
                let cell_id_for_focus = cell_id.clone();
                cx.subscribe_in(
                    code_cell,
                    window,
                    move |this, _cell, event, window, cx| match event {
                        CellEvent::Run(cell_id) => this.execute_cell(cell_id.clone(), window, cx),
                        CellEvent::FocusedIn(_) => this.select_cell_by_id(&cell_id_for_focus, cx),
                    },
                )
                .detach();

                let editor = code_cell.read(cx).editor().clone();
                cx.subscribe(&editor, move |this, _editor, event, cx| {
                    if let editor::EditorEvent::Focused = event {
                        this.select_cell_by_id(&cell_id, cx);
                    }
                })
                .detach();
            }
            Cell::Markdown(markdown_cell) => {
                cx.subscribe(
                    markdown_cell,
                    move |_this, cell, event: &MarkdownCellEvent, cx| match event {
                        MarkdownCellEvent::FinishedEditing | MarkdownCellEvent::Run(_) => {
                            cell.update(cx, |cell, cx| {
                                cell.reparse_markdown(cx);
                            });
                        }
                    },
                )
                .detach();

                let editor = markdown_cell.read(cx).editor().clone();
                cx.subscribe(&editor, move |this, _editor, event, cx| match event {
                    editor::EditorEvent::Focused => this.select_cell_by_id(&cell_id, cx),
                    editor::EditorEvent::BufferEdited => {
                        cx.emit(SearchEvent::MatchesInvalidated);
                        this.update_markdown_diffs(cx);
                    }
                    _ => {}
                })
                .detach();
            }
            Cell::Raw(_) => {}
        }
    }

    /// Rewrites the shared code buffer from the code cells in their current
    /// order, after cells were added, removed or reordered, and points each cell
    /// editor at its new range.
    fn rebuild_code_document(&mut self, cx: &mut Context<Self>) {
        let sources: Vec<String> = self
            .code_cells_in_order()
            .iter()
            .map(|code_cell| code_cell.read(cx).current_source(cx))
            .collect();
        self.rewrite_code_document(sources, cx);
    }

    fn code_cells_in_order(&self) -> Vec<Entity<super::CodeCell>> {
        self.cell_order
            .iter()
            .filter_map(|cell_id| match self.cell_map.get(cell_id) {
                Some(Cell::Code(code_cell)) => Some(code_cell.clone()),
                _ => None,
            })
            .collect()
    }

    /// Like `rebuild_code_document`, but with `sources` as the text of the code
    /// cells, in order.
    fn rewrite_code_document(&mut self, sources: Vec<String>, cx: &mut Context<Self>) {
        let code_cells = self.code_cells_in_order();
        let layout = layout_code_cells(sources.iter().map(String::as_str), &self.comment_prefix);

        self.code_document.update(cx, |document, cx| {
            document.set_text(layout.text, cx);
        });
        for (code_cell, range) in code_cells.iter().zip(layout.cell_ranges) {
            let document = self.code_document.clone();
            code_cell.update(cx, |code_cell, cx| {
                code_cell.set_document_range(document, range, cx);
            });
        }
        cx.notify();
    }

    fn script_cells(&self, cx: &App) -> Vec<ScriptCell> {
        self.cell_order
            .iter()
            .filter_map(|cell_id| self.cell_map.get(cell_id))
            .map(|cell| ScriptCell {
                kind: match cell {
                    Cell::Code(_) => ScriptCellKind::Code,
                    Cell::Markdown(_) => ScriptCellKind::Markdown,
                    Cell::Raw(_) => ScriptCellKind::Raw,
                },
                source: cell.current_source(cx),
            })
            .collect()
    }

    fn is_in_source_view(&self) -> bool {
        self.source_view.is_some()
    }

    fn toggle_source_view(
        &mut self,
        _: &ToggleSourceView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.source_view.is_some() {
            self.apply_source_view(window, cx);
            self.source_view = None;
            cx.emit(SearchEvent::MatchesInvalidated);
            self.notebook_mode = NotebookMode::Command;
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }

        let script = cells_to_script(&self.script_cells(cx), &self.comment_prefix);
        let language = self.code_document.read(cx).language().cloned();
        let languages = self.languages.clone();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(script.clone(), cx);
            buffer.set_language_registry(languages);
            buffer.set_language(language, cx);
            buffer
        });
        let project = self.project.clone();
        let editor = cx.new(|cx| {
            let multi_buffer = cx.new(|cx| editor::MultiBuffer::singleton(buffer, cx));
            editor::Editor::new(
                editor::EditorMode::full(),
                multi_buffer,
                Some(project),
                window,
                cx,
            )
        });
        window.focus(&editor.focus_handle(cx), cx);
        cx.subscribe(&editor, |_, _, event, cx| {
            if let editor::EditorEvent::BufferEdited = event {
                cx.emit(SearchEvent::MatchesInvalidated);
            }
        })
        .detach();
        self.source_view = Some(SourceView {
            editor,
            applied_script: script,
        });
        cx.emit(SearchEvent::MatchesInvalidated);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn source_view_has_edits(&self, cx: &App) -> bool {
        self.source_view.as_ref().is_some_and(|source_view| {
            source_view.editor.read(cx).text(cx) != source_view.applied_script
        })
    }

    /// Rebuilds the cells from the source view's script. Cells that line up with
    /// a cell of the same kind keep their id and outputs, so editing the script
    /// doesn't throw away results.
    fn apply_source_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source_view) = self.source_view.as_mut() else {
            return;
        };
        let script = source_view.editor.read(cx).text(cx);
        if script == source_view.applied_script {
            return;
        }
        source_view.applied_script = script.clone();

        let new_cells = script_to_cells(&script, &self.comment_prefix);
        let old_cells = self.script_cells(cx);
        let reused = align_cells(&old_cells, &new_cells);

        let mut cell_order = Vec::with_capacity(new_cells.len());
        let mut cell_map = HashMap::default();
        let mut code_sources = Vec::new();
        for (new_cell, old_index) in new_cells.iter().zip(reused) {
            if new_cell.kind == ScriptCellKind::Code {
                code_sources.push(new_cell.source.clone());
            }
            let reused_cell = old_index
                .and_then(|index| self.cell_order.get(index))
                .and_then(|cell_id| Some((cell_id.clone(), self.cell_map.get(cell_id)?.clone())));
            if let Some((cell_id, cell)) = reused_cell {
                match &cell {
                    Cell::Markdown(markdown_cell) => {
                        if markdown_cell.read(cx).current_source(cx) != new_cell.source {
                            markdown_cell.update(cx, |markdown_cell, cx| {
                                markdown_cell.set_source(&new_cell.source, window, cx)
                            });
                        }
                    }
                    Cell::Raw(raw_cell) => {
                        raw_cell.update(cx, |raw_cell, _| raw_cell.set_source(&new_cell.source));
                    }
                    // Code cells get their text when the shared buffer is rewritten.
                    Cell::Code(_) => {}
                }
                cell_order.push(cell_id.clone());
                cell_map.insert(cell_id, cell);
            } else {
                let cell_id: CellId = Uuid::new_v4().into();
                let nbformat_cell = new_nbformat_cell(cell_id.clone(), new_cell);
                let cell = Cell::load(
                    &nbformat_cell,
                    &self.languages,
                    &self.code_document,
                    None,
                    &self.project,
                    window,
                    cx,
                );
                self.subscribe_to_cell(cell_id.clone(), &cell, window, cx);
                cell_order.push(cell_id.clone());
                cell_map.insert(cell_id, cell);
            }
        }

        self.cell_order = cell_order;
        self.cell_map = cell_map;
        self.selected_cell_index = self
            .selected_cell_index
            .min(self.cell_order.len().saturating_sub(1));
        self.cell_list = ListState::new(self.cell_order.len(), gpui::ListAlignment::Top, px(1000.));
        self.rewrite_code_document(code_sources, cx);
        self.update_markdown_diffs(cx);
    }

    fn has_structural_changes(&self) -> bool {
        self.cell_order != self.original_cell_order
    }

    fn has_content_changes(&self, cx: &App) -> bool {
        self.cell_map.values().any(|cell| cell.is_dirty(cx))
    }

    pub fn to_notebook(&self, cx: &App) -> nbformat::v4::Notebook {
        let cells: Vec<nbformat::v4::Cell> = self
            .cell_order
            .iter()
            .filter_map(|cell_id| {
                self.cell_map
                    .get(cell_id)
                    .map(|cell| cell.to_nbformat_cell(cx))
            })
            .collect();

        let metadata = self.notebook_item.read(cx).notebook.metadata.clone();

        nbformat::v4::Notebook {
            metadata,
            nbformat: 4,
            nbformat_minor: 5,
            cells,
        }
    }

    pub fn mark_as_saved(&mut self, cx: &mut Context<Self>) {
        self.original_cell_order = self.cell_order.clone();

        for cell in self.cell_map.values() {
            match cell {
                Cell::Code(code_cell) => {
                    code_cell.update(cx, |code_cell, cx| code_cell.mark_as_saved(cx));
                }
                Cell::Markdown(markdown_cell) => {
                    markdown_cell.update(cx, |markdown_cell, cx| {
                        let editor = markdown_cell.editor();
                        editor.update(cx, |editor, cx| {
                            editor.buffer().update(cx, |buffer, cx| {
                                if let Some(buf) = buffer.as_singleton() {
                                    buf.update(cx, |b, cx| {
                                        let version = b.version();
                                        b.did_save(version, None, cx);
                                    });
                                }
                            });
                        });
                    });
                }
                Cell::Raw(raw_cell) => {
                    raw_cell.update(cx, |raw_cell, _| raw_cell.mark_as_saved());
                }
            }
        }
        cx.notify();
    }

    fn save_impl(
        &mut self,
        destination: SaveDestination,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let mut notebook = self.to_notebook(cx);
        if JupyterSettings::get_global(cx).clear_outputs_on_save {
            for cell in &mut notebook.cells {
                if let nbformat::v4::Cell::Code {
                    outputs,
                    execution_count,
                    ..
                } = cell
                {
                    outputs.clear();
                    *execution_count = None;
                }
            }
        }
        let project_path = self.notebook_item.read(cx).project_path.clone();
        let json = match self
            .notebook_item
            .update(cx, |item, _| item.file.serialize(&notebook))
            .context("Failed to serialize notebook")
        {
            Ok(json) => json,
            Err(error) => return Task::ready(Err(error)),
        };

        self.mark_as_saved(cx);

        cx.spawn(async move |this, cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            buffer.update(cx, |buffer, cx| buffer.set_text(json, cx));

            match destination {
                SaveDestination::CurrentPath => {
                    project
                        .update(cx, |project, cx| project.save_buffer(buffer, cx))
                        .await
                }
                SaveDestination::NewPath(new_path) => {
                    project
                        .update(cx, |project, cx| {
                            project.save_buffer_as(buffer, new_path.clone(), cx)
                        })
                        .await?;

                    // The buffer now lives at the new path, so the notebook has
                    // to follow it or the next save writes to the old file.
                    let entry_id = project.read_with(cx, |project, cx| {
                        project.entry_for_path(&new_path, cx).map(|entry| entry.id)
                    });
                    this.update(cx, |this, cx| {
                        this.notebook_item.update(cx, |notebook_item, _| {
                            notebook_item.project_path = new_path;
                            if let Some(entry_id) = entry_id {
                                notebook_item.id = entry_id;
                            }
                        })
                    })
                }
            }
        })
    }

    fn launch_kernel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self
            .kernel_specification
            .clone()
            .or_else(|| self.preferred_kernelspec(cx));

        let spec = spec.unwrap_or_else(|| {
            KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "python3".to_string(),
                path: PathBuf::from("python3"),
                kernelspec: JupyterKernelspec {
                    argv: vec![
                        "python3".to_string(),
                        "-m".to_string(),
                        "ipykernel_launcher".to_string(),
                        "-f".to_string(),
                        "{connection_file}".to_string(),
                    ],
                    display_name: "Python 3".to_string(),
                    language: "python".to_string(),
                    interrupt_mode: None,
                    metadata: None,
                    env: None,
                },
            })
        });

        self.launch_kernel_with_spec(spec, window, cx);
    }

    fn launch_kernel_with_spec(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity_id = cx.entity_id();
        let working_directory = self
            .project
            .read(cx)
            .worktree_for_id(self.worktree_id, cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let fs = self.project.read(cx).fs().clone();
        let view = cx.entity();

        self.kernel_specification = Some(spec.clone());

        let kernel_task = match spec {
            KernelSpecification::Jupyter(local_spec) => NativeRunningKernel::new(
                local_spec,
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::PythonEnv(env_spec) => NativeRunningKernel::new(
                env_spec.as_local_spec(),
                entity_id,
                working_directory,
                fs,
                view,
                window,
                cx,
            ),
            KernelSpecification::JupyterServer(remote_spec) => {
                RemoteRunningKernel::new(remote_spec, working_directory, view, window, cx)
            }

            KernelSpecification::SshRemote(spec) => {
                let project = self.project.clone();
                SshRunningKernel::new(spec, working_directory, project, view, window, cx)
            }
            KernelSpecification::WslRemote(spec) => {
                WslRunningKernel::new(spec, entity_id, working_directory, fs, view, window, cx)
            }
        };

        let pending_kernel = cx
            .spawn_in(window, async move |this, cx| {
                let kernel = kernel_task.await;

                match kernel {
                    Ok(kernel) => {
                        this.update_in(cx, |editor, window, cx| {
                            editor.kernel = Kernel::RunningKernel(kernel);
                            editor.run_queued_executions(window, cx);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(err) => {
                        log::error!("Kernel failed to start: {:?}", err);
                        this.update_in(cx, |editor, window, cx| {
                            let error = err.to_string();
                            editor.fail_queued_executions(&error, window, cx);
                            editor.kernel = Kernel::ErroredLaunch(error);
                            cx.notify();
                        })
                        .ok();
                    }
                }
            })
            .shared();

        self.kernel = Kernel::StartingKernel(pending_kernel);
        cx.notify();
    }

    // Note: Python environments are only detected as kernels if ipykernel is installed.
    // Users need to run `pip install ipykernel` (or `uv pip install ipykernel`) in their
    // virtual environment for it to appear in the kernel selector.
    // This happens because we have an ipykernel check inside the function python_env_kernel_specification in mod.rs L:121

    fn change_kernel(
        &mut self,
        spec: KernelSpecification,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &mut self.kernel {
            kernel.force_shutdown(window, cx).detach();
        }

        self.execution_requests.clear();
        self.remember_kernel_choice(&spec, cx);
        self.launch_kernel_with_spec(spec, window, cx);
    }

    fn restart_kernel(&mut self, _: &RestartKernel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(spec) = self.kernel_specification.clone() {
            if let Kernel::RunningKernel(kernel) = &mut self.kernel {
                kernel.force_shutdown(window, cx).detach();
            }

            self.kernel = Kernel::Restarting;
            cx.notify();

            self.launch_kernel_with_spec(spec, window, cx);
        }
    }

    fn interrupt_kernel(
        &mut self,
        _: &InterruptKernel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &self.kernel {
            let interrupt_request = jupyter_protocol::InterruptRequest {};
            let message: JupyterMessage = interrupt_request.into();
            kernel.request_tx().try_send(message).ok();
            cx.notify();
        }
    }

    fn execute_cell(&mut self, cell_id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let code = if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            cell.read(cx).current_source(cx)
        } else {
            return;
        };

        // A kernel that is still starting is slow, not broken: run the cell once
        // it is up instead of failing it.
        if matches!(self.kernel, Kernel::StartingKernel(_) | Kernel::Restarting) {
            if !self.queued_executions.contains(&cell_id) {
                self.queued_executions.push(cell_id.clone());
                if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
                    cell.update(cx, |cell, cx| {
                        cell.clear_outputs();
                        cell.start_execution();
                        cx.notify();
                    });
                }
            }
            return;
        }

        let request = ExecuteRequest {
            code,
            ..Default::default()
        };
        let message: JupyterMessage = request.into();
        let msg_id = message.header.msg_id.clone();

        let send_result = match &mut self.kernel {
            Kernel::RunningKernel(kernel) => kernel
                .request_tx()
                .try_send(message)
                .map_err(|err| format!("failed to send execute request to kernel (the kernel process may have died): {err}")),
            Kernel::StartingKernel(_) | Kernel::Restarting => {
                Err("the kernel is still starting".to_string())
            }
            Kernel::ErroredLaunch(error) => Err(format!("the kernel failed to launch: {error}")),
            Kernel::ShuttingDown | Kernel::Shutdown => Err("the kernel is shut down".to_string()),
        };

        if let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) {
            cell.update(cx, |cell, cx| {
                if cell.has_outputs() {
                    cell.clear_outputs();
                }
                if let Err(error) = &send_result {
                    cell.show_kernel_error(error, window, cx);
                } else {
                    cell.start_execution();
                }
                cx.notify();
            });
        }

        if let Err(error) = send_result {
            log::error!("notebook: cannot execute cell: {error}");
        } else {
            self.execution_requests.insert(msg_id, cell_id.clone());
        }
    }

    fn get_selected_cell(&self) -> Option<&Cell> {
        self.cell_order
            .get(self.selected_cell_index)
            .and_then(|cell_id| self.cell_map.get(cell_id))
    }

    fn has_outputs(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.cell_map.values().any(|cell| {
            if let Cell::Code(code_cell) = cell {
                code_cell.read(cx).has_outputs()
            } else {
                false
            }
        })
    }

    fn clear_outputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for cell in self.cell_map.values() {
            if let Cell::Code(code_cell) = cell {
                code_cell.update(cx, |cell, cx| {
                    cell.clear_outputs();
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    fn run_cells(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_source_view(window, cx);
        for cell_id in self.cell_order.clone() {
            self.execute_cell(cell_id, window, cx);
        }
    }

    fn run_current_cell(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        let Some(cell) = self.cell_map.get(&cell_id) else {
            return;
        };
        match cell {
            Cell::Code(_) => {
                self.execute_cell(cell_id, window, cx);
            }
            Cell::Markdown(markdown_cell) => {
                // for markdown, finish editing and move to next cell
                let is_editing = markdown_cell.read(cx).is_editing();
                if is_editing {
                    markdown_cell.update(cx, |cell, cx| {
                        cell.run(cx);
                    });
                    self.enter_command_mode(window, cx);
                }
            }
            Cell::Raw(_) => {}
        }
    }

    fn run_and_advance(&mut self, _: &RunAndAdvance, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() {
            if let Some(cell) = self.cell_map.get(&cell_id) {
                match cell {
                    Cell::Code(_) => {
                        self.execute_cell(cell_id, window, cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        if markdown_cell.read(cx).is_editing() {
                            markdown_cell.update(cx, |cell, cx| {
                                cell.run(cx);
                            });
                        }
                    }
                    Cell::Raw(_) => {}
                }
            }
        }

        let is_last_cell = self.selected_cell_index == self.cell_count().saturating_sub(1);
        if is_last_cell {
            self.add_code_block(window, cx);
            self.enter_command_mode(window, cx);
        } else {
            self.advance_in_command_mode(window, cx);
        }
    }

    fn enter_edit_mode(&mut self, _: &EnterEditMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        self.notebook_mode = NotebookMode::Edit;
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
            if let Some(cell) = self.cell_map.get(cell_id) {
                match cell {
                    Cell::Code(code_cell) => {
                        let editor = code_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        markdown_cell.update(cx, |cell, cx| {
                            cell.set_editing(true);
                            cx.notify();
                        });
                        let editor = markdown_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Raw(_) => {}
                }
            }
        }
        cx.notify();
    }

    fn enter_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn handle_enter_command_mode(
        &mut self,
        _: &EnterCommandMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enter_command_mode(window, cx);
    }

    /// Advances to the next cell while staying in command mode (used by RunAndAdvance and shift-enter).
    fn advance_in_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.cell_count();
        if count == 0 {
            return;
        }
        if self.selected_cell_index < count - 1 {
            self.selected_cell_index += 1;
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    // Discussion can be done on this default implementation
    /// Moves focus to the next cell editor (used when already in edit mode).
    fn move_to_next_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.cell_order.is_empty() && self.selected_cell_index < self.cell_order.len() - 1 {
            self.selected_cell_index += 1;
            // focus the new cell's editor
            if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
                if let Some(cell) = self.cell_map.get(cell_id) {
                    match cell {
                        Cell::Code(code_cell) => {
                            let editor = code_cell.read(cx).editor();
                            window.focus(&editor.focus_handle(cx), cx);
                        }
                        Cell::Markdown(markdown_cell) => {
                            // Don't auto-enter edit mode for next markdown cell
                            // Just select it
                        }
                        Cell::Raw(_) => {}
                    }
                }
            }
            cx.notify();
        } else {
            // in the end, could optionally create a new cell
            // For now, just stay on the current cell
        }
    }

    fn open_notebook(&mut self, _: &OpenNotebook, _window: &mut Window, _cx: &mut Context<Self>) {
        println!("Open notebook triggered");
    }

    fn move_cell_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The cells are stale while the source view has edits; these act on
        // cells, so they wait until the cell view is back.
        if self.is_in_source_view() {
            return;
        }
        println!("Move cell up triggered");
        if self.selected_cell_index > 0 {
            self.cell_order
                .swap(self.selected_cell_index, self.selected_cell_index - 1);
            self.selected_cell_index -= 1;
            self.rebuild_code_document(cx);
        }
    }

    fn move_cell_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        println!("Move cell down triggered");
        if !self.cell_order.is_empty() && self.selected_cell_index < self.cell_order.len() - 1 {
            self.cell_order
                .swap(self.selected_cell_index, self.selected_cell_index + 1);
            self.selected_cell_index += 1;
            self.rebuild_code_document(cx);
        }
    }

    fn delete_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        if self.cell_order.is_empty() {
            return;
        }
        let index = self.selected_cell_index.min(self.cell_order.len() - 1);
        let cell_id = self.cell_order.remove(index);
        let removed_code_cell = matches!(self.cell_map.remove(&cell_id), Some(Cell::Code(_)));
        self.cell_list.splice(index..index + 1, 0);
        if removed_code_cell {
            self.rebuild_code_document(cx);
        }

        if self.cell_order.is_empty() {
            self.selected_cell_index = 0;
        } else {
            self.selected_cell_index = index.min(self.cell_order.len() - 1);
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn insert_cell_at_current_position(&mut self, cell_id: CellId, cell: Cell) {
        let insert_index = if self.cell_order.is_empty() {
            0
        } else {
            self.selected_cell_index + 1
        };
        self.cell_order.insert(insert_index, cell_id.clone());
        self.cell_map.insert(cell_id, cell);
        self.selected_cell_index = insert_index;
        self.cell_list.splice(insert_index..insert_index, 1);
        self.cell_list.scroll_to_reveal_item(insert_index);
    }

    fn add_markdown_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        let new_cell_id: CellId = Uuid::new_v4().into();
        let languages = self.languages.clone();
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");

        let markdown_cell = cx.new(|cx| {
            super::MarkdownCell::new(
                new_cell_id.clone(),
                metadata,
                String::new(),
                languages,
                window,
                cx,
            )
        });

        self.subscribe_to_cell(
            new_cell_id.clone(),
            &Cell::Markdown(markdown_cell.clone()),
            window,
            cx,
        );

        self.insert_cell_at_current_position(new_cell_id, Cell::Markdown(markdown_cell.clone()));
        markdown_cell.update(cx, |cell, cx| {
            cell.set_editing(true);
            cx.notify();
        });
        let editor = markdown_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.update_markdown_diffs(cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn add_code_block(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_in_source_view() {
            return;
        }
        let new_cell_id: CellId = Uuid::new_v4().into();
        let metadata: nbformat::v4::CellMetadata =
            serde_json::from_str("{}").expect("empty object should parse");
        let code_document = self.code_document.clone();
        let project = self.project.clone();

        let code_cell = cx.new(|cx| {
            super::CodeCell::new(
                super::CellSource::None,
                new_cell_id.clone(),
                metadata,
                code_document,
                None,
                Some(project),
                window,
                cx,
            )
        });
        self.subscribe_to_cell(
            new_cell_id.clone(),
            &Cell::Code(code_cell.clone()),
            window,
            cx,
        );

        self.insert_cell_at_current_position(new_cell_id, Cell::Code(code_cell.clone()));
        self.rebuild_code_document(cx);
        let editor = code_cell.read(cx).editor().clone();
        window.focus(&editor.focus_handle(cx), cx);
        self.notebook_mode = NotebookMode::Edit;
        cx.notify();
    }

    fn cell_count(&self) -> usize {
        self.cell_map.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_cell_index
    }

    fn select_cell_by_id(&mut self, cell_id: &CellId, cx: &mut Context<Self>) {
        if let Some(index) = self.cell_order.iter().position(|id| id == cell_id) {
            self.selected_cell_index = index;
            self.notebook_mode = NotebookMode::Edit;
            cx.emit(ItemEvent::UpdateBreadcrumbs);
            cx.notify();
        }
    }

    pub fn set_selected_index(
        &mut self,
        index: usize,
        jump_to_index: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_cell_index = index;
        let current_index = self.selected_cell_index;
        cx.emit(ItemEvent::UpdateBreadcrumbs);

        if jump_to_index {
            self.jump_to_cell(current_index, window, cx);
        }
    }

    fn select_next(
        &mut self,
        _: &menu::SelectNext,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == count - 1 {
                count - 1
            } else {
                index + 1
            };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::Start, window, cx);
            }

            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == 0 { 0 } else { index - 1 };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::End, window, cx);
            }

            cx.notify();
        }
    }

    pub fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(0, true, window, cx);
            cx.notify();
        }
    }

    pub fn select_last(
        &mut self,
        _: &menu::SelectLast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(count - 1, true, window, cx);
            cx.notify();
        }
    }

    fn jump_to_cell(&mut self, index: usize, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cell_list.scroll_to_reveal_item(index);
    }

    /// Records the selected cell in the navigation history, so that going back
    /// after a jump returns to it.
    fn push_to_nav_history(&mut self, cx: &mut Context<Self>) {
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push(Some(cell_id), None, cx);
        }
    }

    /// Selects and scrolls to the cell at `index`, as a jump that "go back"
    /// returns from.
    pub(super) fn reveal_cell(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.cell_order.len() {
            return;
        }
        self.push_to_nav_history(cx);
        self.set_selected_index(index, true, window, cx);
        self.push_to_nav_history(cx);
        self.enter_command_mode(window, cx);
    }

    fn toggle_outline(
        &mut self,
        _: &zed_actions::outline::ToggleOutline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.source_view.is_some() {
            cx.propagate();
            return;
        }
        let Some(workspace) = self
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.upgrade())
        else {
            return;
        };
        let entries = self
            .cell_order
            .iter()
            .filter_map(|cell_id| self.cell_map.get(cell_id))
            .map(|cell| {
                super::outline::cell_label(
                    &cell.current_source(cx),
                    matches!(cell, Cell::Markdown(_)),
                )
            })
            .collect();
        let delegate = super::outline::NotebookOutlineDelegate::new(
            cx.entity().downgrade(),
            entries,
            self.selected_cell_index,
        );
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                Picker::uniform_list(delegate, window, cx)
            });
        });
    }

    fn button_group(window: &mut Window, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .items_center()
            .w(px(CONTROL_SIZE + 4.0))
            .overflow_hidden()
            .rounded(px(5.))
            .bg(cx.theme().colors().title_bar_background)
            .p_px()
            .border_1()
            .border_color(cx.theme().colors().border)
    }

    fn render_notebook_control(
        id: impl Into<SharedString>,
        icon: IconName,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> IconButton {
        let id: ElementId = ElementId::Name(id.into());
        IconButton::new(id, icon).width(px(CONTROL_SIZE))
    }

    fn render_notebook_controls(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_outputs = self.has_outputs(window, cx);

        v_flex()
            .max_w(px(CONTROL_SIZE + 4.0))
            .items_center()
            .gap(DynamicSpacing::Base16.rems(cx))
            .justify_between()
            .flex_none()
            .h_full()
            .py(DynamicSpacing::Base12.px(cx))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "run-all-cells",
                                    IconName::PlayFilled,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Execute all cells", &RunAll, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(RunAll), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "clear-all-outputs",
                                    IconName::ListX,
                                    window,
                                    cx,
                                )
                                .disabled(!has_outputs)
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Clear all outputs", &ClearOutputs, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ClearOutputs), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-up",
                                    IconName::ArrowUp,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Move cell up", &MoveCellUp, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellUp), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "move-cell-down",
                                    IconName::ArrowDown,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Move cell down", &MoveCellDown, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(MoveCellDown), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx)
                            .child(
                                Self::render_notebook_control(
                                    "new-markdown-cell",
                                    IconName::Plus,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Add markdown block", &AddMarkdownBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddMarkdownBlock), cx);
                                }),
                            )
                            .child(
                                Self::render_notebook_control(
                                    "new-code-cell",
                                    IconName::Code,
                                    window,
                                    cx,
                                )
                                .tooltip(move |window, cx| {
                                    Tooltip::for_action("Add code block", &AddCodeBlock, cx)
                                })
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(AddCodeBlock), cx);
                                }),
                            ),
                    )
                    .child(
                        Self::button_group(window, cx).child(
                            Self::render_notebook_control(
                                "delete-cell",
                                IconName::Trash,
                                window,
                                cx,
                            )
                            .disabled(self.cell_order.is_empty())
                            .tooltip(move |window, cx| {
                                Tooltip::for_action("Delete cell", &DeleteCell, cx)
                            })
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(DeleteCell), cx);
                            }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.rems(cx))
                    .items_center()
                    .child(
                        Self::render_notebook_control("more-menu", IconName::Ellipsis, window, cx)
                            .tooltip(move |window, cx| (Tooltip::text("More options"))(window, cx)),
                    )
                    .child(Self::button_group(window, cx).child({
                        let kernel_status = self.kernel.status();
                        let (icon, icon_color) = match &kernel_status {
                            KernelStatus::Idle => (IconName::ReplNeutral, Color::Success),
                            KernelStatus::Busy => (IconName::ReplNeutral, Color::Warning),
                            KernelStatus::Starting => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Error => (IconName::ReplNeutral, Color::Error),
                            KernelStatus::ShuttingDown => (IconName::ReplNeutral, Color::Muted),
                            KernelStatus::Shutdown => (IconName::ReplNeutral, Color::Disabled),
                            KernelStatus::Restarting => (IconName::ReplNeutral, Color::Warning),
                        };
                        let kernel_name = self
                            .kernel_specification
                            .as_ref()
                            .map(|spec| spec.name().to_string())
                            .unwrap_or_else(|| "Select Kernel".to_string());
                        IconButton::new("repl", icon)
                            .icon_color(icon_color)
                            .tooltip(move |window, cx| {
                                Tooltip::text(format!(
                                    "{} ({}). Click to change kernel.",
                                    kernel_name,
                                    kernel_status.to_string()
                                ))(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.kernel_picker_handle.toggle(window, cx);
                            }))
                    })),
            )
    }

    fn render_kernel_status_bar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let kernel_status = self.kernel.status();
        let kernel_name = self
            .kernel_specification
            .as_ref()
            .map(|spec| spec.name().to_string())
            .unwrap_or_else(|| "Select Kernel".to_string());

        let (status_icon, status_color) = match &kernel_status {
            KernelStatus::Idle => (IconName::Circle, Color::Success),
            KernelStatus::Busy => (IconName::ArrowCircle, Color::Warning),
            KernelStatus::Starting => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Error => (IconName::XCircle, Color::Error),
            KernelStatus::ShuttingDown => (IconName::ArrowCircle, Color::Muted),
            KernelStatus::Shutdown => (IconName::Circle, Color::Muted),
            KernelStatus::Restarting => (IconName::ArrowCircle, Color::Warning),
        };

        let is_spinning = matches!(
            kernel_status,
            KernelStatus::Busy
                | KernelStatus::Starting
                | KernelStatus::ShuttingDown
                | KernelStatus::Restarting
        );

        let status_icon_element = if is_spinning {
            Icon::new(status_icon)
                .size(IconSize::Small)
                .color(status_color)
                .with_rotate_animation(2)
                .into_any_element()
        } else {
            Icon::new(status_icon)
                .size(IconSize::Small)
                .color(status_color)
                .into_any_element()
        };

        let worktree_id = self.worktree_id;
        let kernel_picker_handle = self.kernel_picker_handle.clone();
        let view = cx.entity().downgrade();

        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .justify_between()
            .bg(cx.theme().colors().status_bar_background)
            .child(
                KernelSelector::new(
                    Box::new(move |spec: KernelSpecification, window, cx| {
                        let change_kernel = {
                            let view = view.clone();
                            move |spec, window: &mut Window, cx: &mut App| {
                                if let Some(view) = view.upgrade() {
                                    view.update(cx, |this, cx| {
                                        this.change_kernel(spec, window, cx);
                                    });
                                }
                            }
                        };
                        if spec.has_ipykernel() {
                            change_kernel(spec, window, cx);
                        } else {
                            crate::repl_editor::install_ipykernel(spec, window, cx, change_kernel)
                                .context("failed to install ipykernel")
                                .log_err();
                        }
                    }),
                    worktree_id,
                    Button::new("kernel-selector", kernel_name.clone())
                        .label_size(LabelSize::Small)
                        .start_icon(
                            Icon::new(status_icon)
                                .size(IconSize::Small)
                                .color(status_color),
                        ),
                    Tooltip::text(format!(
                        "Kernel: {} ({}). Click to change.",
                        kernel_name,
                        kernel_status.to_string()
                    )),
                )
                .with_handle(kernel_picker_handle),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("restart-kernel", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(|window, cx| {
                                Tooltip::for_action("Restart Kernel", &RestartKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.restart_kernel(&RestartKernel, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("interrupt-kernel", IconName::Stop)
                            .icon_size(IconSize::Small)
                            .disabled(!matches!(kernel_status, KernelStatus::Busy))
                            .tooltip(|window, cx| {
                                Tooltip::for_action("Interrupt Kernel", &InterruptKernel, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.interrupt_kernel(&InterruptKernel, window, cx);
                            })),
                    ),
            )
    }

    fn cell_list(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        list(self.cell_list.clone(), move |index, window, cx| {
            view.update(cx, |this, cx| {
                let cell_id = &this.cell_order[index];
                let cell = this.cell_map.get(cell_id).unwrap();
                this.render_cell(index, cell, window, cx).into_any_element()
            })
        })
        .size_full()
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(Label::new("This notebook is empty.").color(Color::Muted))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("empty-state-add-code", "Add code cell")
                            .start_icon(Icon::new(IconName::Code))
                            .key_binding(KeyBinding::for_action_in(
                                &AddCodeBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_code_block(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("empty-state-add-markdown", "Add markdown cell")
                            .style(ButtonStyle::Subtle)
                            .start_icon(Icon::new(IconName::FileMarkdown))
                            .key_binding(KeyBinding::for_action_in(
                                &AddMarkdownBlock,
                                &self.focus_handle,
                                cx,
                            ))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_markdown_block(window, cx)
                            })),
                    ),
            )
    }

    fn cell_position(&self, index: usize) -> CellPosition {
        match index {
            0 => CellPosition::First,
            index if index == self.cell_count() - 1 => CellPosition::Last,
            _ => CellPosition::Middle,
        }
    }

    fn render_cell(
        &self,
        index: usize,
        cell: &Cell,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cell_position = self.cell_position(index);

        let is_selected = index == self.selected_cell_index;

        match cell {
            Cell::Code(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Markdown(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Raw(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
        }
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("NotebookEditor");
        key_context.set(
            "notebook_mode",
            match self.notebook_mode {
                NotebookMode::Command => "command",
                NotebookMode::Edit => "edit",
            },
        );
        key_context.set(
            "notebook_view",
            if self.is_in_source_view() {
                "source"
            } else {
                "cells"
            },
        );

        v_flex()
            .size_full()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &OpenNotebook, window, cx| {
                this.open_notebook(&OpenNotebook, window, cx)
            }))
            .on_action(cx.listener(Self::toggle_source_view))
            .on_action(cx.listener(Self::toggle_outline))
            .on_action(
                cx.listener(|this, _: &ClearOutputs, window, cx| this.clear_outputs(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &Run, window, cx| this.run_current_cell(&Run, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.run_and_advance(action, window, cx)),
            )
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_cells(window, cx)))
            .on_action(
                cx.listener(|this, _: &MoveCellUp, window, cx| this.move_cell_up(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveCellDown, window, cx| this.move_cell_down(window, cx)),
            )
            .on_action(cx.listener(|this, _: &AddMarkdownBlock, window, cx| {
                this.add_markdown_block(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &AddCodeBlock, window, cx| this.add_code_block(window, cx)),
            )
            .on_action(cx.listener(|this, _: &DeleteCell, window, cx| this.delete_cell(window, cx)))
            .on_action(
                cx.listener(|this, action, window, cx| this.enter_edit_mode(action, window, cx)),
            )
            .on_action(cx.listener(|this, action, window, cx| {
                this.handle_enter_command_mode(action, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_next(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_previous(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(|this, _: &MoveDown, window, cx| {
                this.select_next(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &MoveUp, window, cx| {
                this.select_previous(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveDown, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_last_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.last() {
                        let head = selection.head();
                        let cursor_row = head.row();
                        let max_row = display_snapshot.max_point().row();

                        cursor_row >= max_row
                    } else {
                        false
                    }
                });

                if is_at_last_line {
                    this.select_next(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_down(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveUp, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_first_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.first() {
                        let head = selection.head();
                        let cursor_row = head.row();

                        cursor_row.0 == 0
                    } else {
                        false
                    }
                });

                if is_at_first_line {
                    this.select_previous(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_up(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action, window, cx| this.restart_kernel(action, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.interrupt_kernel(action, window, cx)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .h_full()
                    .gap_2()
                    .child(div().flex_1().h_full().child(
                        if let Some(source_view) = &self.source_view {
                            source_view.editor.clone().into_any_element()
                        } else if self.cell_order.is_empty() {
                            self.render_empty_state(cx).into_any_element()
                        } else {
                            self.cell_list(window, cx).into_any_element()
                        },
                    ))
                    .when(!self.is_in_source_view(), |this| {
                        this.child(self.render_notebook_controls(window, cx))
                    }),
            )
            .child(self.render_kernel_status_bar(window, cx))
    }
}

impl Focusable for NotebookEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// Intended to be a NotebookBuffer
pub struct NotebookItem {
    project_path: ProjectPath,
    languages: Arc<LanguageRegistry>,
    // Raw notebook data
    notebook: nbformat::v4::Notebook,
    /// The JSON the notebook was last read from or written to.
    file: NotebookFile,
    // Store our version of the notebook in memory (cell_order, cell_map)
    id: ProjectEntryId,
}

impl project::ProjectItem for NotebookItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        let path = path.clone();
        let project = project.clone();
        let languages = project.read(cx).languages().clone();

        // For single-file worktrees the relative path is empty, so fall back
        // to the absolute path to detect notebooks opened directly.
        let abs_path = project.read(cx).absolute_path(&path, cx);
        let is_notebook = path.path.extension().unwrap_or_default() == NOTEBOOK_EXTENSION
            || abs_path
                .as_ref()
                .and_then(|abs_path| abs_path.extension())
                .is_some_and(|extension| extension == NOTEBOOK_EXTENSION);

        if is_notebook {
            Some(cx.spawn(async move |cx| {
                // todo: watch for changes to the file
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                    .await?;
                let file_content = buffer.read_with(cx, |buffer, _| buffer.text());
                let (notebook, file) = NotebookFile::parse(&file_content)?;

                let id = project
                    .update(cx, |project, cx| {
                        project.entry_for_path(&path, cx).map(|entry| entry.id)
                    })
                    .context("Entry not found")?;

                Ok(cx.new(|_| NotebookItem {
                    project_path: path,
                    languages,
                    notebook,
                    file,
                    id,
                }))
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        Some(self.id)
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        // TODO: Track if notebook metadata or structure has changed
        false
    }
}

impl NotebookItem {
    pub fn language_name(&self) -> Option<String> {
        self.notebook
            .metadata
            .language_info
            .as_ref()
            .map(|l| l.name.clone())
            .or(self
                .notebook
                .metadata
                .kernelspec
                .as_ref()
                .and_then(|spec| spec.language.clone()))
    }

    pub fn notebook_language(&self) -> impl Future<Output = Option<Arc<Language>>> + use<> {
        let language_name = self.language_name();
        let languages = self.languages.clone();

        async move {
            if let Some(language_name) = language_name {
                languages.language_for_name(&language_name).await.ok()
            } else {
                None
            }
        }
    }
}

impl EventEmitter<()> for NotebookItem {}

impl EventEmitter<ItemEvent> for NotebookEditor {}

impl EventEmitter<SearchEvent> for NotebookEditor {}

/// A search match in one of the notebook's editors: a cell's, or the source
/// view's when `cell_id` is `None`.
#[derive(Clone)]
pub struct NotebookSearchMatch {
    cell_id: Option<CellId>,
    range: std::ops::Range<editor::Anchor>,
}

impl NotebookEditor {
    /// The editors that search looks through, in notebook order.
    fn searchable_editors(&self, cx: &App) -> Vec<(Option<CellId>, Entity<editor::Editor>)> {
        if let Some(source_view) = &self.source_view {
            return vec![(None, source_view.editor.clone())];
        }
        self.cell_order
            .iter()
            .filter_map(|cell_id| {
                let editor = self.cell_map.get(cell_id)?.editor(cx)?.clone();
                Some((Some(cell_id.clone()), editor))
            })
            .collect()
    }

    fn editor_for_match(
        &self,
        search_match: &NotebookSearchMatch,
        cx: &App,
    ) -> Option<Entity<editor::Editor>> {
        self.searchable_editors(cx)
            .into_iter()
            .find(|(cell_id, _)| *cell_id == search_match.cell_id)
            .map(|(_, editor)| editor)
    }

    /// The matches of each searchable editor, keeping only editors that have some.
    fn matches_by_editor(
        &self,
        matches: &[NotebookSearchMatch],
        cx: &App,
    ) -> Vec<(
        Option<CellId>,
        Entity<editor::Editor>,
        Vec<std::ops::Range<editor::Anchor>>,
    )> {
        self.searchable_editors(cx)
            .into_iter()
            .map(|(cell_id, editor)| {
                let ranges = matches
                    .iter()
                    .filter(|search_match| search_match.cell_id == cell_id)
                    .map(|search_match| search_match.range.clone())
                    .collect();
                (cell_id, editor, ranges)
            })
            .collect()
    }
}

impl SearchableItem for NotebookEditor {
    type Match = NotebookSearchMatch;

    fn supported_options(&self) -> SearchOptions {
        SearchOptions {
            case: true,
            word: true,
            regex: true,
            replacement: true,
            selection: false,
            select_all: false,
            find_in_results: false,
        }
    }

    fn clear_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (_, editor) in self.searchable_editors(cx) {
            editor.update(cx, |editor, cx| editor.clear_matches(window, cx));
        }
    }

    fn update_matches(
        &mut self,
        matches: &[Self::Match],
        active_match_index: Option<usize>,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active_match = active_match_index.and_then(|index| matches.get(index));
        for (cell_id, editor, ranges) in self.matches_by_editor(matches, cx) {
            let active_index = active_match
                .filter(|active_match| active_match.cell_id == cell_id)
                .and_then(|active_match| {
                    ranges.iter().position(|range| *range == active_match.range)
                });
            editor.update(cx, |editor, cx| {
                if ranges.is_empty() {
                    editor.clear_matches(window, cx);
                } else {
                    editor.update_matches(&ranges, active_index, token, window, cx);
                }
            });
        }
    }

    fn query_suggestion(
        &mut self,
        seed_query_override: Option<SeedQuerySetting>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        let editor = match &self.source_view {
            Some(source_view) => Some(source_view.editor.clone()),
            None => self
                .get_selected_cell()
                .and_then(|cell| cell.editor(cx).cloned()),
        };
        editor.map_or_else(String::new, |editor| {
            editor.update(cx, |editor, cx| {
                editor.query_suggestion(seed_query_override, window, cx)
            })
        })
    }

    fn activate_match(
        &mut self,
        index: usize,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(active_match) = matches.get(index) else {
            return;
        };
        if let Some(cell_id) = &active_match.cell_id
            && let Some(cell_index) = self.cell_order.iter().position(|id| id == cell_id)
        {
            if cell_index != self.selected_cell_index {
                self.push_to_nav_history(cx);
            }
            self.selected_cell_index = cell_index;
            cx.emit(ItemEvent::UpdateBreadcrumbs);
            self.cell_list.scroll_to_reveal_item(cell_index);
            // A rendered markdown cell doesn't show its text, so the match could not
            // be seen without switching it to editing.
            if let Some(Cell::Markdown(markdown_cell)) = self.cell_map.get(cell_id) {
                markdown_cell.update(cx, |markdown_cell, cx| {
                    markdown_cell.set_editing(true);
                    cx.notify();
                });
            }
        }
        for (cell_id, editor, ranges) in self.matches_by_editor(matches, cx) {
            if cell_id != active_match.cell_id {
                continue;
            }
            if let Some(local_index) = ranges.iter().position(|range| *range == active_match.range)
            {
                editor.update(cx, |editor, cx| {
                    editor.activate_match(local_index, &ranges, token, window, cx)
                });
            }
        }
        cx.notify();
    }

    fn select_matches(
        &mut self,
        matches: &[Self::Match],
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (_, editor, ranges) in self.matches_by_editor(matches, cx) {
            if !ranges.is_empty() {
                editor.update(cx, |editor, cx| {
                    editor.select_matches(&ranges, token, window, cx)
                });
            }
        }
    }

    fn replace(
        &mut self,
        search_match: &Self::Match,
        query: &SearchQuery,
        token: SearchToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.editor_for_match(search_match, cx) {
            editor.update(cx, |editor, cx| {
                editor.replace(&search_match.range, query, token, window, cx)
            });
        }
    }

    fn find_matches(
        &mut self,
        query: Arc<SearchQuery>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Vec<Self::Match>> {
        let searches: Vec<_> = self
            .searchable_editors(cx)
            .into_iter()
            .map(|(cell_id, editor)| {
                let task = editor.update(cx, |editor, cx| {
                    editor.find_matches(query.clone(), window, cx)
                });
                (cell_id, task)
            })
            .collect();
        cx.background_spawn(async move {
            let mut matches = Vec::new();
            for (cell_id, task) in searches {
                matches.extend(task.await.into_iter().map(|range| NotebookSearchMatch {
                    cell_id: cell_id.clone(),
                    range,
                }));
            }
            matches
        })
    }

    fn active_match_index(
        &mut self,
        _direction: Direction,
        matches: &[Self::Match],
        _token: SearchToken,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        if self.source_view.is_some() {
            return Some(0);
        }
        // Start from the first match at or after the selected cell.
        let selected_cell = self.selected_cell_index;
        let first_after_selection = matches.iter().position(|search_match| {
            search_match
                .cell_id
                .as_ref()
                .and_then(|cell_id| self.cell_order.iter().position(|id| id == cell_id))
                .is_some_and(|cell_index| cell_index >= selected_cell)
        });
        Some(first_after_selection.unwrap_or(0))
    }
}

// pub struct NotebookControls {
//     pane_focused: bool,
//     active_item: Option<Box<dyn ItemHandle>>,
//     // subscription: Option<Subscription>,
// }

// impl NotebookControls {
//     pub fn new() -> Self {
//         Self {
//             pane_focused: false,
//             active_item: Default::default(),
//             // subscription: Default::default(),
//         }
//     }
// }

// impl EventEmitter<ToolbarItemEvent> for NotebookControls {}

// impl Render for NotebookControls {
//     fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
//         div().child("notebook controls")
//     }
// }

// impl ToolbarItemView for NotebookControls {
//     fn set_active_pane_item(
//         &mut self,
//         active_pane_item: Option<&dyn workspace::ItemHandle>,
//         window: &mut Window, cx: &mut Context<Self>,
//     ) -> workspace::ToolbarItemLocation {
//         cx.notify();
//         self.active_item = None;

//         let Some(item) = active_pane_item else {
//             return ToolbarItemLocation::Hidden;
//         };

//         ToolbarItemLocation::PrimaryLeft
//     }

//     fn pane_focus_update(&mut self, pane_focused: bool, _window: &mut Window, _cx: &mut Context<Self>) {
//         self.pane_focused = pane_focused;
//     }
// }

impl Item for NotebookEditor {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.workspace = Some(workspace.weak_handle());
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let file_name = self.tab_content_text(0, cx);
        let mut segments = vec![HighlightedText {
            text: file_name,
            highlights: Vec::new(),
        }];
        if self.source_view.is_some() {
            segments.push(HighlightedText {
                text: "Source".into(),
                highlights: Vec::new(),
            });
        } else if let Some(cell) = self.get_selected_cell() {
            let label = super::outline::cell_label(
                &cell.current_source(cx),
                matches!(cell, Cell::Markdown(_)),
            );
            let position = format!(
                "Cell {} of {}",
                self.selected_cell_index + 1,
                self.cell_order.len()
            );
            segments.push(HighlightedText {
                text: if label.is_empty() {
                    position.into()
                } else {
                    format!("{position}: {label}").into()
                },
                highlights: Vec::new(),
            });
        }
        Some((segments, None))
    }

    fn navigate(
        &mut self,
        data: Arc<dyn std::any::Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(cell_id) = data.downcast_ref::<CellId>() else {
            return false;
        };
        let Some(index) = self.cell_order.iter().position(|id| id == cell_id) else {
            return false;
        };
        self.selected_cell_index = index;
        self.cell_list.scroll_to_reveal_item(index);
        self.enter_command_mode(window, cx);
        cx.emit(ItemEvent::UpdateBreadcrumbs);
        true
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| {
            Self::new(self.project.clone(), self.notebook_item.clone(), window, cx)
        })))
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.notebook_item.entity_id(), self.notebook_item.read(cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.notebook_item
            .read(cx)
            .project_path
            .path
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
            .into()
    }

    fn tab_content(&self, params: TabContentParams, window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(IconName::Book.into())
    }

    fn show_toolbar(&self) -> bool {
        true
    }

    // TODO
    fn pixel_position_of_cursor(&self, _: &App) -> Option<Point<Pixels>> {
        None
    }

    // TODO
    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn set_nav_history(
        &mut self,
        nav_history: workspace::ItemNavHistory,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.nav_history = Some(nav_history);
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.apply_source_view(window, cx);
        self.save_impl(SaveDestination::CurrentPath, project, cx)
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.apply_source_view(window, cx);
        self.save_impl(SaveDestination::NewPath(path), project, cx)
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let project_path = self.notebook_item.read(cx).project_path.clone();

        cx.spawn_in(window, async move |this, cx| {
            let buffer = this
                .update(cx, |this, cx| {
                    this.project
                        .update(cx, |project, cx| project.open_buffer(project_path, cx))
                })?
                .await?;

            let file_content = buffer.read_with(cx, |buffer, _| buffer.text());
            let (notebook, file) = NotebookFile::parse(&file_content)?;

            this.update_in(cx, |this, window, cx| {
                this.notebook_item.update(cx, |item, _| {
                    item.notebook.metadata = notebook.metadata.clone();
                    item.file = file;
                });
                this.replace_cells(&notebook.cells, window, cx);
            })?;

            Ok(())
        })
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.has_structural_changes()
            || self.has_content_changes(cx)
            || self.source_view_has_edits(cx)
    }
}

impl ProjectItem for NotebookEditor {
    type Item = NotebookItem;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(project, item, window, cx)
    }
}

impl KernelSession for NotebookEditor {
    fn route(&mut self, message: &JupyterMessage, window: &mut Window, cx: &mut Context<Self>) {
        // Handle kernel status updates (these are broadcast to all)
        if let JupyterMessageContent::Status(status) = &message.content {
            self.kernel.set_execution_state(&status.execution_state);
            cx.notify();
        }

        if let JupyterMessageContent::KernelInfoReply(reply) = &message.content {
            self.kernel.set_kernel_info(reply);

            if let Ok(language_info) = serde_json::from_value::<nbformat::v4::LanguageInfo>(
                serde_json::to_value(&reply.language_info).unwrap(),
            ) {
                self.notebook_item.update(cx, |item, cx| {
                    item.notebook.metadata.language_info = Some(language_info);
                    cx.emit(());
                });
            }
            cx.notify();
        }

        // Handle cell-specific messages
        if let Some(parent_header) = &message.parent_header {
            if let Some(cell_id) = self.execution_requests.get(&parent_header.msg_id) {
                if let Some(Cell::Code(cell)) = self.cell_map.get(cell_id) {
                    cell.update(cx, |cell, cx| {
                        cell.handle_message(message, window, cx);
                    });
                }
            }
        }
    }

    fn kernel_errored(&mut self, error_message: String, cx: &mut Context<Self>) {
        self.kernel = Kernel::ErroredLaunch(error_message);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, ProjectItem as _};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use util::rel_path::rel_path;

    fn code_cell_at(
        notebook_editor: &Entity<NotebookEditor>,
        index: usize,
        cx: &mut gpui::VisualTestContext,
    ) -> Entity<super::super::CodeCell> {
        notebook_editor.read_with(cx, |notebook_editor, _| {
            let Some(Cell::Code(cell)) = notebook_editor
                .cell_order
                .get(index)
                .and_then(|cell_id| notebook_editor.cell_map.get(cell_id))
            else {
                panic!("expected a code cell at index {index}");
            };
            cell.clone()
        })
    }

    const NOTEBOOK_WITH_ONE_CODE_CELL: &str = r#"{
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {
                "cell_type": "code",
                "id": "cell-one",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["print('hello')"]
            }
        ]
    }"#;

    #[test]
    fn test_upgrade_notebook_repairs_missing_cell_ids() -> anyhow::Result<()> {
        let mut value: serde_json::Value = serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL)?;
        value["cells"][0]
            .as_object_mut()
            .context("cell is not an object")?
            .remove("id");
        let notebook = nbformat::parse_notebook(&serde_json::to_string(&value)?)?;
        assert!(matches!(notebook, nbformat::Notebook::V4QuirksMode(_)));
        let repaired = crate::notebook::file_format::upgrade_notebook(notebook)?;
        let serialized = nbformat::serialize_notebook(&nbformat::Notebook::V4(repaired))?;
        assert!(matches!(
            nbformat::parse_notebook(&serialized)?,
            nbformat::Notebook::V4(_)
        ));
        let saved: serde_json::Value = serde_json::from_str(&serialized)?;
        assert_eq!(saved["cells"][0]["source"], value["cells"][0]["source"]);
        assert!(
            saved["cells"][0]["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        Ok(())
    }

    #[test]
    fn test_upgrade_notebook_preserves_cell_ids() -> anyhow::Result<()> {
        let notebook = crate::notebook::file_format::upgrade_notebook(nbformat::parse_notebook(
            NOTEBOOK_WITH_ONE_CODE_CELL,
        )?)?;
        let value = serde_json::to_value(notebook)?;
        assert_eq!(value["cells"][0]["id"], "cell-one");
        Ok(())
    }

    /// When the configured interpreter doesn't exist (e.g. Python isn't installed),
    /// running a cell must not leave it stuck in the executing state. It should
    /// instead surface the kernel launch error as an error output on the cell.
    #[gpui::test]
    async fn test_run_cell_with_missing_interpreter_shows_error(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        // Select a kernel whose interpreter doesn't exist, simulating a machine
        // where Python isn't installed properly. This is the same path the
        // kernel picker uses.
        let missing_interpreter = path!("/nonexistent/python3");
        let broken_spec = KernelSpecification::Jupyter(LocalKernelSpecification {
            name: "python3".to_string(),
            path: PathBuf::from(missing_interpreter),
            kernelspec: JupyterKernelspec {
                argv: vec![
                    missing_interpreter.to_string(),
                    "-m".to_string(),
                    "ipykernel_launcher".to_string(),
                    "-f".to_string(),
                    "{connection_file}".to_string(),
                ],
                display_name: "Python 3".to_string(),
                language: "python".to_string(),
                interrupt_mode: None,
                metadata: None,
                env: None,
            },
        });
        cx.update(|cx| {
            ReplStore::global(cx).update(cx, |store, cx| {
                store.set_active_kernelspec(worktree_id, broken_spec, cx);
            })
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(
                    &project,
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("test.ipynb").into(),
                    },
                    cx,
                )
                .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Don't render the notebook UI itself: its animated kernel status icon
        // schedules a new frame on every render, which makes `run_until_parked`
        // spin forever in tests. The editor entity is created inside an empty
        // window instead; we are testing execution behavior, not rendering.
        let cx = cx.add_empty_window();

        // Launching a kernel probes real TCP ports on localhost, which the
        // deterministic test scheduler cannot drive.
        cx.executor().allow_parking();

        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        // Creating the editor discovers environments and then launches the kernel.
        // Wait for both, the launch failing because the interpreter cannot be
        // spawned.
        assert!(
            editor.read_with(cx, |editor, _| matches!(
                editor.kernel,
                Kernel::StartingKernel(_)
            )),
            "kernel should be starting right after the editor is created"
        );
        while let Some(pending_kernel) = editor.read_with(cx, |editor, _| match &editor.kernel {
            Kernel::StartingKernel(task) => Some(task.clone()),
            _ => None,
        }) {
            pending_kernel.await;
        }

        editor.read_with(cx, |editor, _| {
            assert!(
                matches!(editor.kernel, Kernel::ErroredLaunch(_)),
                "kernel launch should fail, instead status is: {}",
                editor.kernel.status().to_string()
            );
        });

        // Run the (only) cell via the production action handler.
        editor.update_in(cx, |editor, window, cx| {
            editor.run_current_cell(&Run, window, cx);
        });

        editor.read_with(cx, |editor, cx| {
            let cell_id = editor.cell_order.first().expect("notebook has one cell");
            let Some(Cell::Code(cell)) = editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            let cell = cell.read(cx);

            assert!(
                !cell.is_executing(),
                "cell must not be stuck in the executing state when the kernel is not running"
            );

            let nbformat::v4::Cell::Code { outputs, .. } = cell.to_nbformat_cell(cx) else {
                panic!("expected a code cell");
            };
            match outputs.as_slice() {
                [nbformat::v4::Output::Error(error)] => {
                    assert_eq!(error.ename, "Kernel Error");
                    let traceback = error.traceback.join("\n");
                    assert!(
                        traceback.contains("the kernel failed to launch"),
                        "error output should explain why the cell could not run, got: {traceback}"
                    );
                }
                other => panic!("expected a single error output, got: {other:?}"),
            }
        });
    }

    /// Opening a notebook as a single file (its own worktree) leaves the
    /// worktree-relative path empty, so only the absolute path carries the
    /// `.ipynb` extension. `try_open` must still recognize it as a notebook.
    #[gpui::test]
    async fn test_open_single_file_notebook(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "single.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project =
            Project::test(fs.clone(), [path!("/notebooks/single.ipynb").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap();
            let worktree = worktree.read(cx);
            assert!(
                worktree.is_single_file(),
                "opening a bare .ipynb should create a single-file worktree"
            );
            ProjectPath {
                worktree_id: worktree.id(),
                path: worktree.root_entry().unwrap().path.clone(),
            }
        });

        assert!(
            project_path.path.extension().is_none(),
            "single-file worktree relative path should have no extension"
        );

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("single-file .ipynb should open as a notebook")
            })
            .await
            .expect("notebook should parse");

        notebook_item.read_with(cx, |item, _| {
            assert_eq!(item.notebook.cells.len(), 1);
        });
    }

    /// Notebooks must be saved through the project rather than through the
    /// client's own filesystem, otherwise a remote notebook's path is resolved
    /// against the local machine and the save fails.
    #[gpui::test]
    async fn test_save_goes_through_the_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Held across the save: a save that bypasses the project writes the file
        // behind this buffer's back, leaving it stale.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })
            .await
            .expect("notebook buffer should open");

        // Rendering the notebook animates the kernel status icon, which makes
        // `run_until_parked` spin forever; only the editor entity is needed here.
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let cell = notebook_editor.read_with(cx, |notebook_editor, _| {
            let cell_id = notebook_editor
                .cell_order
                .first()
                .expect("notebook has one cell");
            let Some(Cell::Code(cell)) = notebook_editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            cell.clone()
        });
        cell.update(cx, |cell, cx| cell.set_source("print('goodbye')", cx));

        notebook_editor
            .update_in(cx, |notebook_editor, window, cx| {
                notebook_editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("saving the notebook should succeed");

        let saved = String::from_utf8(
            fs.read_file_sync(path!("/notebooks/test.ipynb"))
                .expect("notebook should still exist"),
        )
        .expect("notebook should be valid UTF-8");
        assert!(
            saved.contains("print('goodbye')"),
            "the edited cell should be written to the notebook, got: {saved}"
        );

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                saved,
                "the project's buffer should hold the saved notebook"
            );
            assert!(!buffer.is_dirty(), "saving should leave the buffer clean");
        });
    }

    /// Saving rewrites only what was edited: outputs the editor can't render, such
    /// as widgets, and the file's own formatting survive a save.
    #[gpui::test]
    async fn test_saving_an_unedited_notebook_keeps_the_file(cx: &mut TestAppContext) {
        const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": 3,
   "id": "widget-cell",
   "metadata": {
    "collapsed": false
   },
   "outputs": [
    {
     "data": {
      "application/vnd.jupyter.widget-view+json": {
       "model_id": "8f1c",
       "version_major": 2,
       "version_minor": 0
      },
      "text/plain": [
       "IntSlider(value=0)"
      ]
     },
     "metadata": {},
     "output_type": "display_data"
    }
   ],
   "source": [
    "import ipywidgets\n",
    "ipywidgets.IntSlider()"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "widgets": {
   "application/vnd.jupyter.widget-state+json": {
    "state": {}
   }
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/notebooks"), json!({ "test.ipynb": NOTEBOOK }))
            .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let save = |cx: &mut gpui::VisualTestContext| {
            notebook_editor.update_in(cx, |notebook_editor, window, cx| {
                notebook_editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
        };
        let read_saved = || {
            String::from_utf8(
                fs.read_file_sync(path!("/notebooks/test.ipynb"))
                    .expect("notebook should still exist"),
            )
            .expect("notebook should be valid UTF-8")
        };

        save(cx).await.expect("saving the notebook should succeed");
        assert_eq!(read_saved(), NOTEBOOK);

        let cell = code_cell_at(&notebook_editor, 0, cx);
        cell.update(cx, |cell, cx| {
            cell.set_source("import ipywidgets\nipywidgets.IntSlider(5)", cx)
        });
        save(cx).await.expect("saving the notebook should succeed");

        assert_eq!(
            read_saved(),
            NOTEBOOK.replace("\"ipywidgets.IntSlider()\"", "\"ipywidgets.IntSlider(5)\""),
            "only the edited source line should change"
        );
    }

    async fn open_notebook_editor<'a>(
        contents: &str,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<NotebookEditor>,
        &'a mut gpui::VisualTestContext,
        Arc<FakeFs>,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/notebooks"), json!({ "test.ipynb": contents }))
            .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        (notebook_editor, cx, fs)
    }

    const NOTEBOOK_WITH_TWO_CODE_CELLS: &str = r#"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "first",
   "metadata": {},
   "outputs": [],
   "source": [
    "import math"
   ]
  },
  {
   "cell_type": "markdown",
   "id": "notes",
   "metadata": {},
   "source": [
    "Some notes"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "second",
   "metadata": {},
   "outputs": [],
   "source": [
    "math.pi"
   ]
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;

    #[gpui::test]
    async fn test_code_cells_are_excerpts_of_one_buffer(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let first = code_cell_at(&notebook_editor, 0, cx);
        let second = code_cell_at(&notebook_editor, 2, cx);
        cx.read(|cx| {
            assert_eq!(
                first.read(cx).document().entity_id(),
                second.read(cx).document().entity_id(),
            );
            assert_eq!(
                first.read(cx).document().read(cx).text(),
                "# %%\nimport math\n# %%\nmath.pi"
            );
            assert_eq!(first.read(cx).current_source(cx), "import math");
            assert_eq!(second.read(cx).current_source(cx), "math.pi");
        });

        second.update(cx, |cell, cx| cell.set_source("print(math.pi)", cx));
        cx.read(|cx| {
            assert_eq!(
                first.read(cx).document().read(cx).text(),
                "# %%\nimport math\n# %%\nprint(math.pi)"
            );
            assert_eq!(first.read(cx).current_source(cx), "import math");
        });
    }

    #[gpui::test]
    async fn test_reordering_and_adding_cells_rewrites_the_shared_buffer(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.selected_cell_index = 2;
            notebook_editor.move_cell_up(window, cx);
            notebook_editor.move_cell_up(window, cx);
        });
        let document =
            code_cell_at(&notebook_editor, 0, cx).read_with(cx, |cell, _| cell.document().clone());
        cx.read(|cx| {
            assert_eq!(document.read(cx).text(), "# %%\nmath.pi\n# %%\nimport math");
        });

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.selected_cell_index = 0;
            notebook_editor.add_code_block(window, cx);
        });
        let added = code_cell_at(&notebook_editor, 1, cx);
        added.update(cx, |cell, cx| cell.set_source("radius = 2", cx));
        cx.read(|cx| {
            assert_eq!(
                document.read(cx).text(),
                "# %%\nmath.pi\n# %%\nradius = 2\n# %%\nimport math"
            );
            assert_eq!(added.read(cx).current_source(cx), "radius = 2");
        });
    }

    #[gpui::test]
    async fn test_sidecar_file_backs_the_code_cells_while_open(cx: &mut TestAppContext) {
        use fs::Fs as _;

        const NOTEBOOK: &str = r#"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "only",
   "metadata": {},
   "outputs": [],
   "source": [
    "value = 1"
   ]
  }
 ],
 "metadata": {
  "language_info": {
   "name": "Python"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"#;
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .editor
                        .jupyter
                        .get_or_insert_default()
                        .language_server_sidecar = Some(true);
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/notebooks"), json!({ "analysis.ipynb": NOTEBOOK }))
            .await;
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project
                .languages()
                .register_test_language(language::LanguageConfig {
                    name: "Python".into(),
                    matcher: language::LanguageMatcher {
                        path_suffixes: vec!["py".into()],
                        ..Default::default()
                    }
                    .into(),
                    line_comments: vec!["# ".into()],
                    ..Default::default()
                });
        });
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("analysis.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        cx.run_until_parked();

        let sidecar = std::path::PathBuf::from(path!("/notebooks/.analysis.ipynb.py"));
        let written = fs
            .load(&sidecar)
            .await
            .expect("the sidecar file should exist while the notebook is open");
        assert_eq!(written, "# %%\nvalue = 1");

        let cell = code_cell_at(&notebook_editor, 0, cx);
        cx.read(|cx| {
            let document = cell.read(cx).document().read(cx);
            assert!(
                document.file().is_some(),
                "cells should be backed by the file"
            );
            assert_eq!(cell.read(cx).current_source(cx), "value = 1");
        });

        drop(cell);
        cx.update(|_, _| drop(notebook_editor));
        cx.run_until_parked();
        assert!(
            !fs.is_file(&sidecar).await,
            "closing the notebook should delete the sidecar file"
        );
    }

    #[gpui::test]
    async fn test_source_view_edits_come_back_as_cells(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.toggle_source_view(&ToggleSourceView, window, cx);
        });
        let source_editor = notebook_editor.read_with(cx, |notebook_editor, _| {
            notebook_editor
                .source_view
                .as_ref()
                .expect("the source view should be open")
                .editor
                .clone()
        });
        cx.read(|cx| {
            assert_eq!(
                source_editor.read(cx).text(cx),
                "# %%\nimport math\n\n# %% [markdown]\n# Some notes\n\n# %%\nmath.pi"
            );
        });

        source_editor.update_in(cx, |source_editor, window, cx| {
            source_editor.set_text(
                "# %%\nimport math\n\n# %% [markdown]\n# Some notes\n\n# %%\nmath.tau\n\n# %%\nprint(1)",
                window,
                cx,
            );
        });
        assert!(notebook_editor.read_with(cx, |notebook_editor, cx| notebook_editor.is_dirty(cx)));

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.toggle_source_view(&ToggleSourceView, window, cx);
        });
        notebook_editor.read_with(cx, |notebook_editor, cx| {
            assert!(notebook_editor.source_view.is_none());
            let ids: Vec<_> = notebook_editor
                .cell_order
                .iter()
                .map(|cell_id| cell_id.to_string())
                .collect();
            assert_eq!(ids.len(), 4);
            assert_eq!(&ids[..3], ["first", "notes", "second"]);
        });
        let edited = code_cell_at(&notebook_editor, 2, cx);
        let added = code_cell_at(&notebook_editor, 3, cx);
        cx.read(|cx| {
            assert_eq!(edited.read(cx).current_source(cx), "math.tau");
            assert_eq!(added.read(cx).current_source(cx), "print(1)");
            assert_eq!(
                edited.read(cx).document().read(cx).text(),
                "# %%\nimport math\n# %%\nmath.tau\n# %%\nprint(1)"
            );
        });
    }

    #[test]
    fn test_align_cells_keeps_unchanged_and_edited_cells() {
        let code = |source: &str| ScriptCell {
            kind: ScriptCellKind::Code,
            source: source.to_string(),
        };
        let markdown = |source: &str| ScriptCell {
            kind: ScriptCellKind::Markdown,
            source: source.to_string(),
        };
        let old = vec![code("a"), markdown("notes"), code("b"), code("c")];
        let new = vec![code("new"), code("a"), markdown("notes"), code("b edited")];
        assert_eq!(
            align_cells(&old, &new),
            vec![None, Some(0), Some(1), Some(2)]
        );

        let reordered = vec![code("c"), code("a")];
        assert_eq!(align_cells(&old, &reordered), vec![Some(3), Some(0)]);
    }

    #[gpui::test]
    async fn test_code_cells_diff_against_the_committed_notebook(cx: &mut TestAppContext) {
        // The same notebook after re-running the first cell and editing the
        // second one.
        let working_copy = NOTEBOOK_WITH_TWO_CODE_CELLS
            .replace(
                "   \"execution_count\": null,\n   \"id\": \"first\",\n   \"metadata\": {},\n   \"outputs\": [],",
                "   \"execution_count\": 7,\n   \"id\": \"first\",\n   \"metadata\": {},\n   \"outputs\": [\n    {\n     \"name\": \"stdout\",\n     \"output_type\": \"stream\",\n     \"text\": [\n      \"done\\n\"\n     ]\n    }\n   ],",
            )
            .replace("\"math.pi\"", "\"math.tau\"");
        assert_ne!(working_copy, NOTEBOOK_WITH_TWO_CODE_CELLS);

        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ ".git": {}, "test.ipynb": working_copy }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            std::path::Path::new(path!("/notebooks/.git")),
            &[("test.ipynb", NOTEBOOK_WITH_TWO_CODE_CELLS.to_string())],
        );
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));
        cx.run_until_parked();

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        cx.run_until_parked();

        let changed_rows = |notebook_editor: &Entity<NotebookEditor>,
                            cx: &mut gpui::VisualTestContext| {
            notebook_editor.read_with(cx, |notebook_editor, cx| {
                let diff = notebook_editor
                    .code_diff
                    .as_ref()
                    .expect("a notebook in a repository should have a diff");
                let document = notebook_editor.code_document.read(cx).text_snapshot();
                diff.read(cx)
                    .snapshot(cx)
                    .hunks(&document)
                    .map(|hunk| {
                        use language::ToPoint as _;
                        hunk.buffer_range.start.to_point(&document).row
                    })
                    .collect::<Vec<_>>()
            })
        };

        // Row 3 holds the second code cell; the first cell's new outputs and
        // execution count are not part of the comparison.
        assert_eq!(changed_rows(&notebook_editor, cx), vec![3]);

        let first = code_cell_at(&notebook_editor, 0, cx);
        first.update(cx, |cell, cx| cell.set_source("import math, os", cx));
        cx.run_until_parked();
        assert_eq!(changed_rows(&notebook_editor, cx), vec![1, 3]);
    }

    #[gpui::test]
    async fn test_search_finds_matches_in_every_cell(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let search = |text: &str,
                      notebook_editor: &Entity<NotebookEditor>,
                      cx: &mut gpui::VisualTestContext| {
            let query = Arc::new(
                SearchQuery::text(
                    text,
                    false,
                    false,
                    false,
                    Default::default(),
                    Default::default(),
                    false,
                    None,
                )
                .expect("the query should be valid"),
            );
            notebook_editor.update_in(cx, |notebook_editor, window, cx| {
                notebook_editor.find_matches(query, window, cx)
            })
        };

        let matches = search("math", &notebook_editor, cx).await;
        let cells: Vec<_> = matches
            .iter()
            .map(|search_match| search_match.cell_id.as_ref().map(|id| id.to_string()))
            .collect();
        assert_eq!(cells, vec![Some("first".into()), Some("second".into())]);

        let matches = search("notes", &notebook_editor, cx).await;
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].cell_id.as_ref().map(|id| id.to_string()),
            Some("notes".into())
        );

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.activate_match(0, &matches, SearchToken::default(), window, cx);
            assert_eq!(notebook_editor.selected_cell_index, 1);
        });
    }

    #[gpui::test]
    async fn test_breadcrumbs_and_navigation_follow_the_selected_cell(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let breadcrumb = |notebook_editor: &Entity<NotebookEditor>,
                          cx: &mut gpui::VisualTestContext| {
            notebook_editor.read_with(cx, |notebook_editor, cx| {
                let (segments, _) = notebook_editor
                    .breadcrumbs(cx)
                    .expect("notebooks have breadcrumbs");
                segments
                    .iter()
                    .map(|segment| segment.text.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            breadcrumb(&notebook_editor, cx),
            vec!["test.ipynb", "Cell 1 of 3: import math"]
        );

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.reveal_cell(1, window, cx);
        });
        assert_eq!(
            breadcrumb(&notebook_editor, cx),
            vec!["test.ipynb", "Cell 2 of 3: Some notes"]
        );

        let first_cell = CellId::new("first").expect("valid cell id");
        let navigated = notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.navigate(Arc::new(first_cell), window, cx)
        });
        assert!(navigated);
        notebook_editor.read_with(cx, |notebook_editor, _| {
            assert_eq!(notebook_editor.selected_cell_index, 0);
        });
    }

    #[gpui::test]
    async fn test_markdown_cells_diff_against_the_committed_notebook(cx: &mut TestAppContext) {
        let working_copy =
            NOTEBOOK_WITH_TWO_CODE_CELLS.replace("\"Some notes\"", "\"Some new notes\"");
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ ".git": {}, "test.ipynb": working_copy }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            std::path::Path::new(path!("/notebooks/.git")),
            &[("test.ipynb", NOTEBOOK_WITH_TWO_CODE_CELLS.to_string())],
        );
        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));
        cx.run_until_parked();

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        cx.run_until_parked();

        notebook_editor.read_with(cx, |notebook_editor, cx| {
            let notes_id = &notebook_editor.cell_order[1];
            let (diff, _) = notebook_editor
                .markdown_diffs
                .get(notes_id)
                .expect("the markdown cell should have a diff");
            let Some(Cell::Markdown(cell)) = notebook_editor.cell_map.get(notes_id) else {
                panic!("expected a markdown cell");
            };
            let buffer = cell
                .read(cx)
                .buffer(cx)
                .expect("markdown cells have a buffer");
            let snapshot = buffer.read(cx).text_snapshot();
            assert_eq!(diff.read(cx).snapshot(cx).hunks(&snapshot).count(), 1);
        });
    }

    fn python_kernelspec(argv0: &str) -> JupyterKernelspec {
        JupyterKernelspec {
            argv: vec![
                argv0.to_string(),
                "-m".to_string(),
                "ipykernel_launcher".to_string(),
                "-f".to_string(),
                "{connection_file}".to_string(),
            ],
            display_name: "Python 3".to_string(),
            language: "python".to_string(),
            interrupt_mode: None,
            metadata: None,
            env: None,
        }
    }

    #[gpui::test]
    async fn test_the_project_environment_is_preferred_over_system_python(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        let system_python = path!("/usr/bin/python3");
        let project_python = path!("/notebooks/.venv/bin/python");
        let specs = vec![
            KernelSpecification::Jupyter(LocalKernelSpecification {
                name: "python3".to_string(),
                path: PathBuf::from(system_python),
                kernelspec: python_kernelspec(system_python),
            }),
            KernelSpecification::PythonEnv(crate::kernels::PythonEnvKernelSpecification {
                name: "Python 3".to_string(),
                path: PathBuf::from(project_python),
                kernelspec: python_kernelspec(project_python),
                has_ipykernel: true,
                environment_kind: Some("venv".to_string()),
            }),
        ];
        cx.update(|_, cx| {
            ReplStore::global(cx).update(cx, |store, cx| {
                store.set_kernel_specs_for_testing(specs, cx)
            })
        });

        let preferred = notebook_editor.read_with(cx, |notebook_editor, cx| {
            notebook_editor
                .preferred_kernelspec(cx)
                .map(|spec| spec.path().to_string())
        });
        assert_eq!(preferred.as_deref(), Some(project_python));
    }

    #[gpui::test]
    async fn test_running_before_the_kernel_is_ready_queues_the_cell(cx: &mut TestAppContext) {
        let (notebook_editor, cx, _fs) =
            open_notebook_editor(NOTEBOOK_WITH_TWO_CODE_CELLS, cx).await;

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.kernel = Kernel::StartingKernel(Task::ready(()).shared());
            let cell_id = notebook_editor.cell_order[0].clone();
            notebook_editor.execute_cell(cell_id.clone(), window, cx);
            notebook_editor.execute_cell(cell_id, window, cx);
            assert_eq!(notebook_editor.queued_executions.len(), 1);
        });
        let cell = code_cell_at(&notebook_editor, 0, cx);
        cx.read(|cx| assert!(cell.read(cx).is_executing()));

        notebook_editor.update_in(cx, |notebook_editor, window, cx| {
            notebook_editor.fail_queued_executions("no interpreter", window, cx);
            assert!(notebook_editor.queued_executions.is_empty());
        });
        cx.read(|cx| {
            assert!(!cell.read(cx).is_executing());
            let nbformat::v4::Cell::Code { outputs, .. } = cell.read(cx).to_nbformat_cell(cx)
            else {
                panic!("expected a code cell");
            };
            assert!(matches!(
                outputs.as_slice(),
                [nbformat::v4::Output::Error(_)]
            ));
        });
    }
}
