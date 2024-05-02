//! Jupyter support for Zed.
use collections::HashMap;
use editor::Editor;
use gpui::{
    impl_actions, AppContext, Context, EventEmitter, FocusHandle, FocusableView, Model, WeakView,
};
use itertools::Itertools;
use language::{self, Buffer};
use project::{self, ProjectPath};
use rope::Bytes;
use runtimelib::media::MimeType;
use serde::de::{self, DeserializeOwned, DeserializeSeed, Visitor};
use serde_derive::Deserialize;
use std::{io::Read, ops::RangeFull, sync::Arc};
use sum_tree::{self, SumTree, Summary};
use ui::{Render, ViewContext};
use worktree::File;

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
#[derive(Default)]
struct Notebook {
    metadata: Option<HashMap<String, String>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: usize,
    nbformat_minor: usize,
    cells: Cells,
}

struct NotebookBuilder<'de> {
    cx: &'de mut AppContext,
    metadata: Option<HashMap<String, String>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: Option<usize>,
    nbformat_minor: Option<usize>,
    cells: Cells,
}

impl<'de> NotebookBuilder<'de> {
    fn new(cx: &'de mut AppContext) -> NotebookBuilder<'de> {
        NotebookBuilder {
            cx,
            metadata: None,
            nbformat: None,
            nbformat_minor: None,
            cells: Cells::default(),
        }
    }

    fn build(self) -> Notebook {
        Notebook {
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

impl<'de> Visitor<'de> for &'de mut NotebookBuilder<'de> {
    type Value = ();

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
                        let cell_builder = CellBuilder::new(&mut self.cx, idx, item);
                        let cell = cell_builder.build();
                        self.cells.0.push(cell, &());
                    }
                }
            }
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for NotebookBuilder<'de> {
    type Value = NotebookBuilder<'de>;

    fn deserialize<D>(mut self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(&mut self)?;
        Ok(self)
    }
}

// TODO: Serde for `Cells`
#[derive(Default)]
struct Cells(SumTree<Cell>);

#[derive(Clone)]
struct Cell {
    // The following should be owned by the underlying Buffer
    // TODO: Make an ID type?
    idx: usize,
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
    idx: usize,
    cell_id: Option<String>,
    cell_type: Option<CellType>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    source: Option<Model<Buffer>>,
    execution_count: Option<usize>,
}

impl<'de> CellBuilder<'de> {
    fn new(
        cx: &mut AppContext,
        idx: usize,
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
                    let source_buf = this.cx.new_model::<Buffer>(|model_cx| {
                        // let cell: Cell = serde_json::from_value(item).map_err(A::Error::custom)?;
                        Buffer::local(text, model_cx)
                    });
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
    file: Option<Arc<dyn language::File>>,
    active_cell: Option<WeakView<Cell>>,
    notebook: Model<Notebook>,
    // focus_handle: FocusHandle,
}

impl project::Item for NotebookEditor {
    fn try_open(
        project: &gpui::Model<project::Project>,
        path: &project::ProjectPath,
        cx: &mut gpui::AppContext,
    ) -> Option<gpui::Task<gpui::Result<gpui::Model<Self>>>>
    where
        Self: Sized,
    {
        if path.path.extension().unwrap() != "ipynb" {
            return None;
        }

        let task = cx.spawn(|mut cx_task| async move {
            let buf = project
                .update(&mut cx_task, |project, model_cx| {
                    project.open_buffer(path.clone(), model_cx)
                })?
                .await?;

            let notebook = cx.new_model(move |cx_model| {
                let bytes = buf.read_with(cx_model, |buf, cx_model| {
                    let mut bytes = Vec::<u8>::with_capacity(buf.len());
                    buf.bytes_in_range(0..buf.len()).read_to_end(&mut bytes);
                    bytes
                });

                let deserializer = serde_json::Deserializer::from_slice(bytes.as_slice());

                NotebookBuilder::new(cx)
                    .deserialize(&mut deserializer)
                    .map(|builder| builder.build())
                    .unwrap_or_default()
            });

            Ok(cx.new_model(move |cx_model| {
                NotebookEditor {
                    file: None,
                    active_cell: None,
                    notebook,
                    // focus_handle:
                }
            }))
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
    type Event = ();

    // fn tab_content(
    //     &self,
    //     _params: workspace::item::TabContentParams,
    //     _cx: &ui::prelude::WindowContext,
    // ) -> gpui::AnyElement {
    // }

    // fn tab_tooltip_text(&self, cx: &AppContext) -> Option<SharedString> {
    //     let file_path = self
    //         .buffer()
    //         .read(cx)
    //         .as_singleton()?
    //         .read(cx)
    //         .file()
    //         .and_then(|f| f.as_local())?
    //         .abs_path(cx);

    //     let file_path = file_path.compact().to_string_lossy().to_string();

    //     Some(file_path.into())
    // }

    fn serialized_item_kind() -> Option<&'static str> {
        Some(NOTEBOOK_KIND)
    }
}

impl EventEmitter<()> for NotebookEditor {}

impl FocusableView for NotebookEditor {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, cx: &mut ui::prelude::ViewContext<Self>) -> impl ui::prelude::IntoElement {
        gpui::Empty
    }
}

impl workspace::item::ProjectItem for NotebookEditor {
    type Item = NotebookEditor;

    // fn for_project_item(
    //     project: gpui::Model<project::Project>,
    //     item: gpui::Model<Self::Item>,
    //     cx: &mut ui::prelude::ViewContext<Self>,
    // ) -> Self
    // where
    //     Self: Sized,
    // {
    //     Self {
    //         focus_handle: cx.focus_handle(),
    //     }
    // }
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
