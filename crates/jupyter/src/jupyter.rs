//! Jupyter support for Zed.
use collections::HashMap;
use editor::{
    items::entry_label_color, Editor, EditorEvent, ExcerptRange, MultiBuffer, MAX_TAB_TITLE_LEN,
};
use gpui::{
    impl_actions, AppContext, Context, EventEmitter, FocusHandle, FocusableView, Model,
    ModelContext, ParentElement, View, WeakView,
};
use itertools::Itertools;
use language::{self, Buffer, Capability};
use project::{self, Project, ProjectPath};
use runtimelib::media::MimeType;
use serde::de::{self, DeserializeOwned, DeserializeSeed, Error, Visitor};
use serde_derive::Deserialize;
use std::{any::Any, io::Read, num::NonZeroU64, ops::Range, sync::Arc};
use sum_tree::{self, SumTree, Summary};
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, ViewContext, VisualContext,
};
use util::paths::PathExt;
use workspace::item::{Item, ItemEvent, ItemHandle};
use worktree::File;

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
#[derive(Default)]
struct Notebook {
    file: Option<Arc<dyn language::File>>,
    metadata: Option<HashMap<String, String>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: usize,
    nbformat_minor: usize,
    cells: Cells,
}

struct NotebookBuilder<'a, 'b: 'a> {
    cx: &'a mut ModelContext<'b, Notebook>,
    file: Option<Arc<dyn language::File>>,
    metadata: Option<HashMap<String, String>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: Option<usize>,
    nbformat_minor: Option<usize>,
    cells: Cells,
}

impl<'a, 'b: 'a> NotebookBuilder<'a, 'b> {
    fn new(
        file: Option<Arc<dyn language::File>>,
        cx: &'a mut ModelContext<'b, Notebook>,
    ) -> NotebookBuilder<'a, 'b> {
        NotebookBuilder {
            cx,
            file,
            metadata: None,
            nbformat: None,
            nbformat_minor: None,
            cells: Cells::default(),
        }
    }

    fn build(self) -> Notebook {
        Notebook {
            file: self.file,
            metadata: self.metadata,
            nbformat: self.nbformat.unwrap(),
            nbformat_minor: self.nbformat_minor.unwrap(),
            cells: self.cells,
        }
    }
}

fn parse_value<T: DeserializeOwned, E: serde::de::Error>(val: serde_json::Value) -> Result<T, E> {
    serde_json::from_value::<T>(val).map_err(|err| E::custom(err.to_string()))
}

impl<'a, 'b, 'de> Visitor<'de> for NotebookBuilder<'a, 'b>
where
    'b: 'a,
    'a: 'de,
{
    type Value = NotebookBuilder<'a, 'b>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            formatter,
            "A document conforming to the Jupyter Notebook specification"
        )
    }

    fn visit_map<A>(mut self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        while let Some((key, val)) = map.next_entry()? {
            match key {
                "metadata" => self.metadata = parse_value(val)?,
                "nbformat" => self.nbformat = parse_value(val)?,
                "nbformat_minor" => self.nbformat_minor = parse_value(val)?,
                "cells" => {
                    let items: Vec<serde_json::Map<String, serde_json::Value>> = parse_value(val)?;
                    for (idx, item) in items.into_iter().enumerate() {
                        let id: CellId = NonZeroU64::new(idx as u64)
                            .ok_or_else(|| A::Error::custom("Nope"))?
                            .into();
                        let cell_builder = CellBuilder::new(&mut self.cx, id, item);
                        let cell = cell_builder.build();
                        self.cells.0.push(cell, &());
                    }
                }
                _ => {}
            }
        }
        Ok(self)
    }
}

impl<'a, 'b, 'de> DeserializeSeed<'de> for NotebookBuilder<'a, 'b>
where
    'b: 'a,
    'a: 'de,
{
    type Value = NotebookBuilder<'a, 'b>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

// TODO: Serde for `Cells`
#[derive(Default)]
struct Cells(SumTree<Cell>);

#[derive(Clone)]
struct Cell {
    // The following should be owned by the underlying Buffer
    // TODO: Make an ID type?
    idx: CellId,
    cell_id: Option<String>,
    cell_type: CellType,
    metadata: HashMap<String, serde_json::Value>,
    source: Model<Buffer>,
    execution_count: Option<usize>,
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#cell-types
#[derive(Clone, Debug, Default, Deserialize)]
enum CellType {
    Raw,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#markdown-cells
    Markdown,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#code-cells
    #[default]
    Code,
}

// TODO: Appropriate deserialize of `source` from `CellSource` (`String`/`Vec<String>`) values
#[derive(Deserialize)]
enum SerializedCellSource {
    String(String),
    MultiLineString(Vec<String>),
}

// We need a replica and `BufferId` to create a `TextBuffer`, and the easiest way
// to pass these to `deserialize` is to implement `DeserializeSeed` for a struct
// containing these data.
struct CellBuilder<'de> {
    cx: &'de mut AppContext,
    idx: CellId,
    cell_id: Option<String>,
    cell_type: Option<CellType>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    source: Option<Model<Buffer>>,
    execution_count: Option<usize>,
}

impl<'de> CellBuilder<'de> {
    fn new(
        cx: &mut AppContext,
        idx: CellId,
        map: serde_json::Map<String, serde_json::Value>,
    ) -> CellBuilder {
        let mut this = CellBuilder {
            cx,
            idx,
            cell_id: None,
            cell_type: None,
            metadata: None,
            source: None,
            execution_count: None,
        };

        for (key, val) in map {
            match key.as_str() {
                "cell_id" => this.cell_id = val.as_str().map(|s| s.to_string()),
                "cell_type" => {
                    this.cell_type = serde_json::from_value(val).unwrap_or_default();
                }
                "metadata" => this.metadata = serde_json::from_value(val).unwrap_or_default(),
                "source" => {
                    let text: String = serde_json::from_value(val).unwrap_or_default();
                    let source_buf = this.cx.new_model(|model_cx| Buffer::local(text, model_cx));
                    this.source.replace(source_buf);
                }
                "execution_count" => {
                    this.execution_count = serde_json::from_value(val).unwrap_or_default()
                }
                _ => {}
            }
        }

        this
    }

    fn build(self) -> Cell {
        Cell {
            idx: self.idx,
            cell_id: self.cell_id,
            cell_type: self.cell_type.unwrap(),
            metadata: self.metadata.unwrap(),
            source: self.source.unwrap(),
            execution_count: self.execution_count,
        }
    }
}

// Fundamentally, a cell is a view into a section of the notebook `Buffer`,
// where the section is delineated by the entry in the `cells` entry.
// The cell should not hold its own data, but rather enable reference
// to the data that exists in and is manipulated by the `Buffer`.
//
// Since a renderable element must be `'static`, the cell view itself must not contain references
// to the underlying buffer, but should rather contain some information that enables seeking
// along the underlying buffer to the relevant data when the view is rendered.
//
// Moreover, since the underlying data must be able to be mutated, we cannot hold a
// reference to the source itself. We also don't want to re-implement rendering for the editor
// functionality of a `Cell`, but rather use an `AutoHeight` editor (e.g. as in `AutoHeightEditorStory`).
// We shouldn't carry around a `BufferSnapshot` that we need to update constantly.
// This all points to carrying an offset that can be used to seek along the underlying
// notebook buffer.
//
// Notes:
// -    `Buffer` is:
//          "An in-memory representation of a source code file, including its text,
//          syntax trees, git status, and diagnostics."
// -    `MultiBuffer` is "One or more [`Buffers`](Buffer) being edited in a single view."
// -    `Editor` is a `View` into a `MultiBuffer` and contains all functionality for displaying
//      and modifying data
// -    Since a notebook is a single file, this naturally suggests that an `Editor` should encapsulate
//      the functionality for modifying a notebook.
// -    However, an `Editor` UI is inconsistent with the notebook cell structure
// -    Can we take inspiration from another context in which we have a single editor view that is
//      segmented over multiple sources?
// -    `ProjectDiagnosticsEditor` is one example, with the significant difference that its source is
//      a `MultiBuffer`
// -    Possibilities:
//          - A single Editor over the notebook Buffer. The

impl sum_tree::Item for Cell {
    type Summary = CellSummary;

    fn summary(&self) -> Self::Summary {
        // let CellSource(source) = self.source;
        CellSummary {
            cell_type: self.cell_type.clone(),
            // text_summary: source.base_text().summary(),
        }
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, PartialOrd, Ord, Eq)]
pub struct CellId(NonZeroU64);

impl From<NonZeroU64> for CellId {
    fn from(id: NonZeroU64) -> Self {
        CellId(id)
    }
}

#[derive(Clone, Debug, Default)]
struct CellSummary {
    cell_type: CellType,
    // text_summary: TextSummary,
}

impl Summary for CellSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, cx: &Self::Context) {}
}

// impl FocusableView for Cell {
//     fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
//         self.focus_handle.clone()
//     }
// }

impl Cell {
    fn handle_focus(&mut self, cx: &mut ViewContext<Self>) {}
}

struct CellView {
    offset: usize,
    focus_handle: FocusHandle,
}

impl Render for CellView {
    fn render(
        &mut self,
        _cx: &mut ui::prelude::ViewContext<Self>,
    ) -> impl ui::prelude::IntoElement {
        gpui::Empty
    }
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#code-cell-outputs
// TODO: Better typing for `output_type`
#[derive(Deserialize)]
enum IpynbCodeOutput {
    Stream {
        output_type: String,
        name: StreamOutputTarget,
        // text: Rope,
        text: String,
    },
    DisplayData {
        output_type: String,
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, HashMap<String, serde_json::Value>>,
    },
    ExecutionResult {
        output_type: String,
        execution_count: usize,
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, HashMap<String, serde_json::Value>>,
    },
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#display-data
#[derive(Deserialize)]
enum MimeData {
    MultiLineText(Vec<String>),
    B64EncodedMultiLineText(Vec<String>),
    Json(HashMap<String, serde_json::Value>),
}

// TODO: Appropriate deserialize from string value
#[derive(Deserialize)]
enum StreamOutputTarget {
    Stdout,
    Stderr,
}

enum JupyterServerEvent {}

// #[derive(Default)]
struct NotebookEditor {
    active_cell: Option<WeakView<Cell>>,
    notebook: Model<Notebook>,
    // editors: HashMap<CellId, View<Editor>>,
    editor: View<Editor>,
    focus_handle: FocusHandle,
}

impl NotebookEditor {
    fn new(project: Model<Project>, notebook: Model<Notebook>, cx: &mut ViewContext<Self>) -> Self {
        let cells = notebook
            .read(cx)
            .cells
            .0
            .iter()
            .map(|cell| {
                let range = ExcerptRange {
                    context: Range {
                        start: 0 as usize,
                        end: cell.source.read(cx).row_count() as usize,
                    },
                    primary: None,
                };
                (cell.source.clone(), range)
            })
            .collect_vec();

        let multi = cx.new_model(|model_cx| {
            let mut multi = MultiBuffer::new(0, Capability::ReadWrite);
            for (source, range) in cells {
                multi.push_excerpts(source, [range].to_vec(), model_cx);
            }
            multi
        });
        let editor = cx.new_view(|cx| {
            let mut editor = Editor::for_multibuffer(multi, Some(project.clone()), cx);
            editor.set_vertical_scroll_margin(5, cx);
            editor
        });

        NotebookEditor {
            active_cell: None,
            notebook,
            editor,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl project::Item for Notebook {
    fn try_open(
        project_handle: &gpui::Model<project::Project>,
        path: &project::ProjectPath,
        app_cx: &mut gpui::AppContext,
    ) -> Option<gpui::Task<gpui::Result<gpui::Model<Self>>>>
    where
        Self: Sized,
    {
        if !path.path.extension().is_some_and(|ext| ext == "ipynb") {
            return None;
        }

        let project = project_handle.downgrade();
        let open_buffer_task =
            project.update(app_cx, |project, cx| project.open_buffer(path.clone(), cx));

        let task = app_cx.spawn(|mut cx| async move {
            let buffer_handle = open_buffer_task?.await?;

            cx.new_model(move |cx_model| {
                let buffer = buffer_handle.read(cx_model);
                let mut bytes = Vec::<u8>::with_capacity(buffer.len());
                let _ = buffer
                    .bytes_in_range(0..buffer.len())
                    .read_to_end(&mut bytes);

                let mut deserializer = serde_json::Deserializer::from_slice(&bytes);

                NotebookBuilder::new(buffer.file().map(|file| file.clone()), cx_model)
                    .deserialize(&mut deserializer)
                    .map(|builder| builder.build())
                    .unwrap_or_default()
            })
        });
        Some(task)
    }

    fn entry_id(&self, cx: &gpui::AppContext) -> Option<project::ProjectEntryId> {
        File::from_dyn(self.file.as_ref()).and_then(|file| file.project_entry_id(cx))
    }

    fn project_path(&self, cx: &gpui::AppContext) -> Option<project::ProjectPath> {
        File::from_dyn(self.file.as_ref()).map(|file| ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path.clone(),
        })
    }
}

const NOTEBOOK_KIND: &'static str = "NotebookEditor";

impl workspace::item::Item for NotebookEditor {
    type Event = EditorEvent;

    fn to_item_events(event: &Self::Event, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut workspace::Workspace,
        cx: &mut ViewContext<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx))
    }

    fn tab_content(
        &self,
        params: workspace::item::TabContentParams,
        cx: &ui::prelude::WindowContext,
    ) -> gpui::AnyElement {
        let title = (&self.notebook).read(cx).file.as_ref().and_then(|f| {
            let path = f.as_local()?.abs_path(cx);
            Some(util::truncate_and_trailoff(
                path.to_str()?,
                MAX_TAB_TITLE_LEN,
            ))
        });

        h_flex()
            .gap_2()
            .when_some(title, |this, title| {
                this.child(
                    Label::new(title)
                        .color(entry_label_color(params.selected))
                        .italic(params.preview),
                )
            })
            .into_any_element()
    }

    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<SharedString> {
        let path = (&self.notebook)
            .read(cx)
            .file
            .as_ref()
            .and_then(|f| f.as_local())?
            .abs_path(cx);

        Some(path.compact().to_string_lossy().to_string().into())
    }

    fn serialized_item_kind() -> Option<&'static str> {
        Some(NOTEBOOK_KIND)
    }
}

impl EventEmitter<EditorEvent> for NotebookEditor {}

impl FocusableView for NotebookEditor {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, cx: &mut ui::prelude::ViewContext<Self>) -> impl ui::prelude::IntoElement {
        let child = self.editor.clone();

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(child)
    }
}

impl workspace::item::ProjectItem for NotebookEditor {
    type Item = Notebook;

    fn for_project_item(
        project: gpui::Model<project::Project>,
        notebook: gpui::Model<Notebook>,
        cx: &mut ui::prelude::ViewContext<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        NotebookEditor::new(project, notebook, cx)
    }
}

pub fn init(cx: &mut AppContext) {
    workspace::register_project_item::<NotebookEditor>(cx);
}

#[derive(Clone, Deserialize, PartialEq)]
pub enum ToggleNotebookView {
    NotebookEditor,
    Raw,
}

impl_actions!(workspace, [ToggleNotebookView]);
