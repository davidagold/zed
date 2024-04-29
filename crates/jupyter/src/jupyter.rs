//! Jupyter support for Zed.
use collections::HashMap;
use editor::Editor;
use gpui::{impl_actions, Context, WeakView};
use language::{self, Buffer, Rope, TextBuffer, TextSummary};
use project::{self, ProjectPath};
use runtimelib::media::MimeType;
use serde_derive::Deserialize;
use std::sync::Arc;
use sum_tree::{self, SumTree, Summary};
use worktree::File;

#[derive(Clone, Deserialize, PartialEq)]
pub enum ToggleNotebookView {
    Notebook,
    Raw,
}

impl_actions!(workspace, [ToggleNotebookView]);

struct NotebookView {}

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
#[derive(Deserialize, Default)]
struct Notebook {
    #[serde(skip)]
    editor: Option<Editor>,
    #[serde(skip)]
    file: Option<Arc<dyn language::File>>,
    #[serde(skip)]
    active_editor: Option<WeakView<Editor>>,
    metadata: Option<HashMap<String, String>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: usize,
    nbformat_minor: usize,
    cells: Cells,
}

// TODO: Serde for `Cells`
struct Cells(SumTree<Cell>);

#[derive(Deserialize, Clone)]
enum Cell {
    // https://nbformat.readthedocs.io/en/latest/format_description.html#cell-types
    Raw {
        cell_type: String,
        metadata: HashMap<String, serde_json::Value>,
        source: TextBuffer,
    },
    // https://nbformat.readthedocs.io/en/latest/format_description.html#markdown-cells
    Markdown {
        cell_type: String,
        metadata: HashMap<String, serde_json::Value>,
        source: TextBuffer,
    },

    // https://nbformat.readthedocs.io/en/latest/format_description.html#code-cells
    Code {
        cell_type: String,
        execution_count: Option<usize>,
        metadata: HashMap<String, serde_json::Value>,
        source: TextBuffer,
    },
}

impl sum_tree::Item for Cell {
    type Summary = CellSummary;

    fn summary(&self) -> Self::Summary {
        use Cell::*;
        let (cell_type, source) = match self {
            Raw {
                cell_type, source, ..
            }
            | Markdown {
                cell_type, source, ..
            }
            | Code {
                cell_type, source, ..
            } => (cell_type, source),
        };
        CellSummary {
            cell_type: cell_type.clone(),
            text_summary: source.base_text().summary(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CellSummary {
    // TODO: Better typing
    cell_type: String,
    text_summary: TextSummary,
}

impl Summary for CellSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, cx: &Self::Context) {}
}

// TODO: Appropriate deserialize of `source` from `CellSource` (`String`/`Vec<String>`) values
// TODO: Better typing for `cell_type`

struct CellSource {
    buffer: Buffer,
}

// Since for the moment we intend to represent a cell source as a `Buffer`, we don't need
// this enum and can instead handle "variants" in the `Serialize`/`Deserialize` implementations.
// enum CellSource {
//     String(String),
//     MultiLineString(Vec<String>),
// }

// https://nbformat.readthedocs.io/en/latest/format_description.html#code-cell-outputs
// TODO: Better typing for `output_type`
#[derive(Deserialize)]
enum IpynbCodeOutput {
    Stream {
        output_type: String,
        name: StreamOutputTarget,
        text: Rope,
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

impl project::Item for Notebook {
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

        let task = cx.spawn(|mut cx| async move {
            let buf = project
                .update(&mut cx, |project, model_cx| {
                    project.open_buffer(path.clone(), model_cx)
                })?
                .await?;

            cx.new_model(move |cx_model| {
                serde_json::from_str(&buf.read(&mut cx_model).as_rope().to_string())
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
