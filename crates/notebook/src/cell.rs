use std::num::NonZeroU64;

use collections::HashMap;
use gpui::{AppContext, Context, FocusHandle, Model};
use language::Buffer;
use runtimelib::media::MimeType;
use serde::Deserialize;
use sum_tree::{SumTree, Summary};
use ui::{Render, ViewContext};

#[derive(Clone)]
pub struct Cell {
    // The following should be owned by the underlying Buffer
    // TODO: Make an ID type?
    idx: CellId,
    cell_id: Option<String>,
    cell_type: CellType,
    metadata: HashMap<String, serde_json::Value>,
    pub source: Model<Buffer>,
    execution_count: Option<usize>,
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#cell-types
#[derive(Clone, Debug, Default, Deserialize)]
pub enum CellType {
    Raw,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#markdown-cells
    Markdown,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#code-cells
    #[default]
    Code,
}

// TODO: Appropriate deserialize of `source` from `CellSource` (`String`/`Vec<String>`) values
#[derive(Deserialize)]
pub enum SerializedCellSource {
    String(String),
    MultiLineString(Vec<String>),
}

// We need a replica and `BufferId` to create a `TextBuffer`, and the easiest way
// to pass these to `deserialize` is to implement `DeserializeSeed` for a struct
// containing these data.
pub struct CellBuilder<'de> {
    cx: &'de mut AppContext,
    idx: CellId,
    cell_id: Option<String>,
    cell_type: Option<CellType>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    source: Option<Model<Buffer>>,
    execution_count: Option<usize>,
}

impl<'de> CellBuilder<'de> {
    pub fn new(
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

    pub fn build(self) -> Cell {
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

#[derive(Default)]
pub struct Cells(SumTree<Cell>);

impl Cells {
    pub fn push_cell(&mut self, cell: Cell, cx: &<CellSummary as Summary>::Context) {
        self.0.push(cell, cx);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cell> {
        self.0.iter()
    }
}

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
pub struct CellSummary {
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
pub enum MimeData {
    MultiLineText(Vec<String>),
    B64EncodedMultiLineText(Vec<String>),
    Json(HashMap<String, serde_json::Value>),
}

// TODO: Appropriate deserialize from string value
#[derive(Deserialize)]
pub enum StreamOutputTarget {
    Stdout,
    Stderr,
}

enum JupyterServerEvent {}
