use collections::HashMap;
use gpui::{AppContext, Context, Flatten, Model, WeakModel};
use language::{Buffer, Language, LanguageConfig, LanguageMatcher};

use project::Project;
use runtimelib::media::MimeType;
use serde::{de::Visitor, Deserialize};
use std::{fmt::Debug, num::NonZeroU64, sync::Arc};
use sum_tree::{SumTree, Summary};
use tree_sitter_python;

#[derive(Clone, Debug)]
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

// Including the `cx: AppContext` in the builder, which is the direct target of deserialization,
// allows us to pass the `cx` along as we deserialize via `DeserializeSeed::deserialize`.
pub struct CellBuilder {
    idx: CellId,
    cell_id: Option<String>,
    cell_type: Option<CellType>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    source: Option<Model<Buffer>>,
    execution_count: Option<usize>,
}

impl Debug for CellBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`cell_id`: {:#?}", self.cell_id)
    }
}

impl CellBuilder {
    pub fn new(
        project_handle: &mut WeakModel<Project>,
        cx: &mut AppContext,
        idx: CellId,
        map: serde_json::Map<String, serde_json::Value>,
    ) -> CellBuilder {
        let mut this = CellBuilder {
            idx,
            cell_id: None,
            cell_type: None,
            metadata: None,
            source: None,
            execution_count: None,
        };

        for (key, val) in map {
            let result_parse_entry = (|| -> anyhow::Result<()> {
                match key.as_str() {
                    "cell_id" => this.cell_id = val.as_str().map(|s| s.to_string()),
                    "cell_type" => {
                        this.cell_type = serde_json::from_value(val).unwrap_or_default();
                    }
                    "metadata" => this.metadata = serde_json::from_value(val).unwrap_or_default(),
                    "source" => {
                        let mut source_lines = Vec::<String>::new();

                        match val {
                            serde_json::Value::String(src) => Ok(source_lines.push(src)),
                            serde_json::Value::Array(src_lines) => {
                                src_lines.into_iter().try_fold((), |_, line_as_val| {
                                    let line = line_as_val.as_str()?;

                                    source_lines
                                        .push(line.strip_suffix("\n").unwrap_or(line).to_string());

                                    Some(())
                                });

                                Ok(())
                            }
                            _ => Err(anyhow::anyhow!("Unexpected source format: {:#?}", val)),
                        }?;

                        project_handle
                            .update(cx, |project, project_cx| -> anyhow::Result<()> {
                                let python_lang = Language::new(
                                    LanguageConfig {
                                        name: "Python".into(),
                                        matcher: LanguageMatcher {
                                            path_suffixes: vec!["rs".to_string()],
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    },
                                    Some(tree_sitter_python::language()),
                                );

                                let source_buffer = project.create_buffer(
                                    source_lines.join("\n").as_str(),
                                    Some(Arc::new(python_lang)),
                                    project_cx,
                                )?;

                                this.source.replace(source_buffer);

                                Ok(())
                            })
                            .flatten()?;
                    }
                    "execution_count" => {
                        this.execution_count = serde_json::from_value(val).unwrap_or_default()
                    }
                    _ => {}
                };

                Ok(())
            })();

            match result_parse_entry {
                Ok(()) => log::info!("Successfully parsed notebook entry with key '{:#?}'", key),
                Err(err) => log::error!(
                    "Failed to parse notebook entry with key '{:#?}': {:#?}",
                    key,
                    err
                ),
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

// https://nbformat.readthedocs.io/en/latest/format_description.html#cell-types
#[derive(Clone, Debug, Default)]
pub enum CellType {
    Raw,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#markdown-cells
    Markdown,
    // https://nbformat.readthedocs.io/en/latest/format_description.html#code-cells
    #[default]
    Code,
}

struct CellTypeVisitor();

impl<'de> Visitor<'de> for CellTypeVisitor {
    type Value = CellType;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "One of 'raw', 'markdown', or 'code'")
    }

    fn visit_str<E>(self, cell_type: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        match cell_type {
            "raw" => Ok(CellType::Raw),
            "markdown" => Ok(CellType::Markdown),
            "code" => Ok(CellType::Code),
            _ => Err(E::custom(format!(
                "Unexpected cell type '{:#?}'",
                cell_type
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for CellType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(CellTypeVisitor())
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
