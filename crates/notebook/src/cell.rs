use crate::{do_in, jupyter::message::Message};
use anyhow::{anyhow, Result};
use collections::HashMap;
use editor::{ExcerptId, ExcerptRange, MultiBuffer};
use gpui::{AppContext, AsyncAppContext, Flatten, Model, ModelContext, WeakModel};
use itertools::Itertools;
use language::{Buffer, Capability, File};
use project::Project;
use rope::Rope;
use runtimelib::media::MimeType;
use serde::{de::Visitor, Deserialize};
use serde_json::Value;
use std::{any::Any, fmt::Debug, path::PathBuf, sync::Arc};
use sum_tree::{Cursor, Dimension, SumTree, Summary};
use ui::Context;

#[derive(Clone, Debug)]
pub struct Cell {
    pub id: CellId,
    // The `msg_id` of the latest `execute_request` message sent to request the execution of the present cell
    pub(crate) latest_execute_request_msg_id: Option<String>,

    pub cell_id: Option<String>, // `cell_id` is a notebook field
    _excerpt_id: std::cell::Cell<ExcerptId>,
    pub cell_type: CellType,
    pub metadata: HashMap<String, serde_json::Value>,
    pub source: Model<Buffer>,
    pub execution_count: Option<usize>,
    pub outputs: Option<Vec<IpynbCodeOutput>>,
    pub output_content: Option<Model<Buffer>>,
}

#[derive(Default)]
pub struct CellBuilder {
    id: u64,
    excerpt_id: std::cell::Cell<ExcerptId>,
    cell_id: Option<String>,
    cell_type: Option<CellType>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    source: Option<Model<Buffer>>,
    execution_count: Option<usize>,
    outputs: Option<Vec<IpynbCodeOutput>>,
    output_content: Option<Model<Buffer>>,
}

impl Debug for CellBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`cell_id`: {:#?}", self.cell_id)
    }
}

impl CellBuilder {
    pub fn new(id: u64) -> CellBuilder {
        CellBuilder {
            id,
            ..CellBuilder::default()
        }
    }

    pub(crate) fn process_map(
        mut self,
        map: serde_json::Map<String, serde_json::Value>,
        project_handle: &WeakModel<Project>,
        cx: &mut AsyncAppContext,
    ) -> Self {
        for (key, val) in map {
            // Work in a closure to propagate errors without returning early
            let result_parse_entry = do_in!(|| -> Result<_> {
                match key.as_str() {
                    "cell_id" => self.cell_id = val.as_str().map(|s| s.to_string()),
                    "cell_type" => {
                        self.cell_type = serde_json::from_value(val).unwrap_or_default();
                    }
                    "metadata" => self.metadata = serde_json::from_value(val).unwrap_or_default(),
                    "source" => {
                        let source_lines: Vec<String> = match val {
                            serde_json::Value::String(src) => Ok([src].into()),
                            serde_json::Value::Array(src_lines) => src_lines.into_iter().try_fold(
                                Vec::<String>::new(),
                                |mut source_lines, line_as_val| {
                                    let line = line_as_val.as_str().ok_or_else(|| {
                                        anyhow!("Source line `{:#?}` is not a string", line_as_val)
                                    })?;
                                    source_lines
                                        .push(line.strip_suffix("\n").unwrap_or(line).to_string());

                                    Ok(source_lines)
                                },
                            ),
                            _ => Err(anyhow!("Unexpected source format: {:#?}", val)),
                        }?;

                        project_handle
                            .update(cx, |project, project_cx| -> Result<()> {
                                let mut source_text = source_lines.join("\n");
                                source_text.push_str("\n");
                                let source_buffer = project.create_buffer(
                                    source_text.as_str(),
                                    None,
                                    project_cx,
                                )?;

                                self.source.replace(source_buffer);
                                Ok(())
                            })
                            .flatten()?;
                    }
                    "execution_count" => {
                        // TODO: Handle this more carefully
                        self.execution_count = serde_json::from_value(val).unwrap_or_default()
                    }
                    "outputs" => {
                        // TODO: Validate `cell_type == 'code'`
                        log::debug!("Cell output value: {:#?}", val);
                        let outputs = serde_json::from_value::<Option<Vec<IpynbCodeOutput>>>(val)?;
                        log::debug!("Parsed cell output as: {:#?}", outputs);

                        // TODO: Organize output-type specific handlers
                        self.outputs = outputs;
                        self.output_content = match self.outputs.as_deref() {
                            Some([]) => None,
                            Some(outputs) => {
                                // TODO: Generic over MIME type and other display options
                                let title = title_for_cell_excerpt(
                                    self.id.into(),
                                    self.cell_id.as_ref(),
                                    self.cell_type.as_ref().unwrap_or(&CellType::Raw),
                                    true,
                                );
                                OutputHandler::try_as_buffer(outputs.iter(), title, cx)
                            }
                            None => None,
                        }
                    }
                    _ => {}
                };

                let cell_id = self.cell_id.clone();
                let cell_type = self.cell_type.clone();
                do_in!(|| -> Option<_> {
                    let title =
                        title_for_cell_excerpt(self.id, (&cell_id).as_ref(), &cell_type?, false);
                    if let Some(buffer_handle) = &self.source {
                        cx.update_model(&buffer_handle, |buffer, cx| {
                            buffer.file_updated(Arc::new(title), cx);
                        })
                        .ok()?;
                    };
                });

                Ok(())
            });

            match result_parse_entry {
                Ok(()) => log::info!("Successfully parsed notebook entry with key '{:#?}'", key),
                Err(err) => log::error!(
                    "Failed to parse notebook entry with key '{:#?}': {:#?}",
                    key,
                    err
                ),
            }
        }

        self
    }

    pub fn build(self) -> Cell {
        Cell {
            id: CellId(self.id),
            latest_execute_request_msg_id: None,
            _excerpt_id: std::cell::Cell::new(self.excerpt_id.take()),
            cell_id: self.cell_id,
            cell_type: self.cell_type.unwrap(),
            metadata: self.metadata.unwrap(),
            source: self.source.unwrap(),
            execution_count: self.execution_count,
            outputs: self.outputs,
            output_content: self.output_content,
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

pub struct Cells {
    pub(crate) tree: SumTree<Cell>,
    pub(crate) multi: Model<MultiBuffer>,
}

impl Cells {
    pub fn iter(&self) -> impl Iterator<Item = &Cell> {
        self.tree.iter()
    }

    pub fn summary(&self) -> &CellSummary {
        self.tree.summary()
    }

    pub fn cursor<'a, D>(&'a self) -> Cursor<'a, Cell, D>
    where
        D: Dimension<'a, CellSummary>,
    {
        self.tree.cursor::<D>()
    }

    pub fn get_by_excerpt_id(&self, excerpt_id: &ExcerptId) -> Option<&Cell> {
        let mut cursor = self.cursor::<ExcerptId>();
        cursor.seek_forward(excerpt_id, text::Bias::Left, &());
        cursor.item()
    }

    pub fn get_cell_by_id(&self, cell_id: &CellId) -> Option<&Cell> {
        let mut cursor = self.cursor::<CellId>();
        cursor.seek_forward(cell_id, text::Bias::Left, &());
        cursor.item()
    }

    fn try_replace_with<F>(&mut self, cell_id: CellId, f: F) -> Result<Cell>
    where
        F: FnOnce(&Cell) -> Result<Cell>,
    {
        let Some(old_cell) = self.tree.get(&cell_id, &()) else {
            let err = anyhow!("Cannot replace nonexistent cell with ID {:#?}", cell_id);
            return Err(err);
        };
        let new_cell = f(old_cell)?;
        if new_cell.id != old_cell.id {
            return Err(anyhow!(
                "New cell ID (`{:#?}`) must match old cell ID (`{:#?}`)",
                new_cell.id,
                old_cell.id
            ));
        };
        self.tree.insert_or_replace(new_cell, &()).ok_or_else(|| {
            anyhow!("Inserted new cell without replacing old, tree is likely corrupted")
        })
    }

    fn insert_cell_after(&mut self, cell_id: CellId, cell: Cell) {
        // TODO
    }

    pub(crate) fn update_cell_from_msg<C: Context>(
        &self,
        cell_id: &CellId,
        msg: Message,
        cx: &mut C,
    ) -> Result<()>
    where
        C::Result<Model<Buffer>>: Into<Model<Buffer>>,
    {
        let Some(cell) = self.get_cell_by_id(cell_id) else {
            return Err(anyhow!("No cell with cell ID {:#?}", cell_id));
        };
        let output_buffer_handle = match cell.output_content.as_ref() {
            Some(buffer_handle) => buffer_handle.clone(),
            None => {
                // TODO
                cx.new_model(|cx| Buffer::local("", cx)).into()
            }
        };

        // TODO: We need to check the parent message ID/execution count to ensure we only clear cell outputs
        //       upon a new execution.
        // cx.update_model(&output_buffer_handle, |buffer, cx| {
        //;
        //     // let excerpt_id = cell.

        //     Ok(())
        // });

        Ok(())
    }

    pub fn from_builders<'c>(
        mut builders: Vec<CellBuilder>,
        cx: &mut AsyncAppContext,
    ) -> Result<Cells> {
        let multi = cx.new_model(|model_cx| {
            let mut multi = MultiBuffer::new(0, Capability::ReadWrite);

            // TODO: Actually guarantee some invariance in `CellId` -> `ExcerptId`.
            let mut prev_excerpt_id = ExcerptId::min();
            for builder in builders.iter_mut() {
                let excerpt_id = ExcerptId::from_proto(prev_excerpt_id.to_proto() + 1);
                // let cell = builder.build(prev_excerpt_id.clone());
                builder.excerpt_id.set(excerpt_id);
                do_in!(|| {
                    let source = builder.source.as_ref()?.clone();
                    let range = excerpt_range_over_buffer(&source, model_cx);
                    multi.insert_excerpts_with_ids_after(
                        prev_excerpt_id,
                        source,
                        vec![(excerpt_id, range)],
                        model_cx,
                    );
                    prev_excerpt_id = excerpt_id;
                });

                if let Some(output_buffer) = &builder.output_content {
                    let excerpt_id = ExcerptId::from_proto(prev_excerpt_id.to_proto() + 1);
                    let range = excerpt_range_over_buffer(&output_buffer, model_cx);
                    multi.insert_excerpts_with_ids_after(
                        prev_excerpt_id,
                        output_buffer.clone(),
                        vec![(excerpt_id, range)],
                        model_cx,
                    );
                    prev_excerpt_id = excerpt_id;
                }
            }

            multi
        })?;

        Ok(Cells {
            tree: SumTree::<Cell>::from_iter(builders.into_iter().map(|b| b.build()), &()),
            multi,
        })
    }
}

impl sum_tree::Item for Cell {
    type Summary = CellSummary;

    fn summary(&self) -> Self::Summary {
        CellSummary {
            trailing_cell_id: self.id,
            trailing_excerpt_id: self._excerpt_id.get().clone(),
        }
    }
}

impl sum_tree::KeyedItem for Cell {
    type Key = CellId;

    fn key(&self) -> Self::Key {
        self.id.clone()
    }
}

impl<'a> Dimension<'a, CellSummary> for CellId {
    fn add_summary(&mut self, _summary: &'a CellSummary, _: &<CellSummary as Summary>::Context) {
        self.0 = std::cmp::max(self.0, _summary.trailing_cell_id.0);
    }
    fn from_summary(summary: &'a CellSummary, cx: &<CellSummary as Summary>::Context) -> Self {
        summary.trailing_cell_id
    }
}

impl<'a> Dimension<'a, CellSummary> for ExcerptId {
    fn add_summary(&mut self, _summary: &'a CellSummary, _: &<CellSummary as Summary>::Context) {
        *self = std::cmp::max(*self, _summary.trailing_excerpt_id)
    }
    fn from_summary(summary: &'a CellSummary, cx: &<CellSummary as Summary>::Context) -> Self {
        summary.trailing_excerpt_id
    }
}

impl<'a> sum_tree::SeekTarget<'a, CellSummary, CellSummary> for CellId {
    fn cmp(
        &self,
        cursor_location: &CellSummary,
        cx: &<CellSummary as Summary>::Context,
    ) -> std::cmp::Ordering {
        Ord::cmp(self, &cursor_location.trailing_cell_id)
    }
}

impl<'a> sum_tree::SeekTarget<'a, CellSummary, CellSummary> for ExcerptId {
    fn cmp(
        &self,
        cursor_location: &CellSummary,
        cx: &<CellSummary as Summary>::Context,
    ) -> std::cmp::Ordering {
        Ord::cmp(self, &cursor_location.trailing_excerpt_id)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CellSummary {
    trailing_cell_id: CellId,
    trailing_excerpt_id: ExcerptId,
}

impl Summary for CellSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, cx: &Self::Context) {
        self.trailing_cell_id.0 =
            std::cmp::max(self.trailing_cell_id.0, summary.trailing_cell_id.0);
        self.trailing_excerpt_id = ExcerptId::from_proto(std::cmp::max(
            self.trailing_excerpt_id.to_proto(),
            summary.trailing_excerpt_id.to_proto(),
        ));
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, PartialOrd, Ord, Eq)]
pub struct CellId(u64);

impl From<u64> for CellId {
    fn from(id: u64) -> Self {
        CellId(id)
    }
}

impl From<ExcerptId> for CellId {
    fn from(excerpt_id: ExcerptId) -> Self {
        CellId(excerpt_id.to_proto())
    }
}

impl Into<u64> for CellId {
    fn into(self) -> u64 {
        self.0
    }
}

impl Into<ExcerptId> for CellId {
    fn into(self) -> ExcerptId {
        ExcerptId::from_proto(self.into())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum StreamOutputText {
    Text(String),
    MultiLineText(Vec<String>),
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#code-cell-outputs
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "output_type")]
pub enum IpynbCodeOutput {
    #[serde(alias = "stream")]
    Stream {
        name: StreamOutputTarget,
        text: StreamOutputText,
    },
    #[serde(alias = "display_data")]
    DisplayData {
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, HashMap<String, serde_json::Value>>,
    },
    #[serde(alias = "execute_result")]
    ExecutionResult {
        execution_count: usize,
        data: HashMap<MimeType, MimeData>,
        metadata: HashMap<MimeType, HashMap<String, serde_json::Value>>,
    },
}

// https://nbformat.readthedocs.io/en/latest/format_description.html#display-data
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum MimeData {
    MultiLineText(Vec<String>),
    B64EncodedMultiLineText(Vec<String>),
    Json(HashMap<String, serde_json::Value>),
}

// TODO: Appropriate deserialize from string value
#[derive(Clone, Debug, Deserialize)]
pub enum StreamOutputTarget {
    #[serde(alias = "stdout")]
    Stdout,
    #[serde(alias = "stderr")]
    Stderr,
}

pub enum JupyterServerEvent {}

// `DisplayMapping`
// For now, we have `Output` x `DisplayMapping` -> `enum Display { Buffer, impl IntoElement }`
// But in general, there's no reason to treat `Buffer` specially.
// We do so currently only because we rely ~100% on the existing editor implementation.
// When notebooks are better integrated, we can `Output` x `DisplayMapping` -> `impl IntoElement`
#[derive(Clone, Debug)]
pub enum OutputHandler {
    Print(Option<String>),
}

// Attempt to decouble cell data model (including MIME output) from the means by which it is displayed.
impl OutputHandler {
    pub fn print(text: Option<StreamOutputText>) -> OutputHandler {
        use StreamOutputText::*;
        match text {
            Some(Text(text)) => OutputHandler::Print(Some(text.to_string())),
            Some(MultiLineText(text)) => {
                let output_text = text
                    .iter()
                    .map(|line| line.strip_suffix("\n").unwrap_or(line).to_string())
                    .join("\n");
                OutputHandler::Print(Some(output_text))
            }
            None => OutputHandler::Print(None),
        }
    }

    pub fn as_rope(&self) -> Option<Rope> {
        let OutputHandler::Print(Some(text)) = self else {
            return None;
        };
        let mut out = Rope::new();
        out.push(text.as_str());
        Some(out)
    }

    pub fn to_media() {
        unimplemented!()
    }

    pub(crate) fn try_as_buffer<'a>(
        outputs: impl Iterator<Item = &'a IpynbCodeOutput>,
        title: PhonyFile,
        cx: &mut AsyncAppContext,
    ) -> Option<Model<Buffer>> {
        // TODO: For MVP we just handle the `stream` output type, for which it is appropriate
        //       to concatenate multiple outputs.
        let content = outputs
            .filter_map(|o| -> Option<OutputHandler> { o.try_into().ok() })
            .map(|h| h.as_rope())
            .fold(Rope::new(), |mut base, content| {
                if content.is_none() {
                    return base;
                };
                base.append(content.unwrap());
                base.append("\n".into());
                base
            });

        cx.new_model(|cx| {
            let mut buffer = Buffer::local(content.to_string(), cx);
            buffer.file_updated(Arc::from(title), cx);
            buffer
        })
        .ok()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct KernelSpec {
    pub argv: Option<Vec<String>>,
    pub display_name: String,
    pub language: String,
    pub interrup_mode: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub metadata: Option<HashMap<String, Value>>,
}

impl TryFrom<&IpynbCodeOutput> for OutputHandler {
    type Error = anyhow::Error;

    fn try_from(output: &IpynbCodeOutput) -> anyhow::Result<Self> {
        use IpynbCodeOutput::*;
        match output {
            Stream { name, text } => {
                log::info!("Output text: {:#?}", text);
                match name {
                    StreamOutputTarget::Stdout => Ok(OutputHandler::print(Some(text.clone()))),
                    StreamOutputTarget::Stderr => Ok(OutputHandler::print(None)),
                }
            }
            DisplayData { data, metadata } => Ok(OutputHandler::print(None)),
            ExecutionResult {
                // TODO: Handle MIME types here
                execution_count,
                data,
                metadata,
            } => Ok(OutputHandler::print(None)),
        }
    }
}

// A phony file struct we use for excerpt titles.
pub(crate) struct PhonyFile {
    worktree_id: usize,
    title: Arc<std::path::Path>,
    cell_idx: CellId,
    cell_id: Option<String>,
}

impl File for PhonyFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn mtime(&self) -> Option<std::time::SystemTime> {
        None
    }

    fn path(&self) -> &Arc<std::path::Path> {
        &self.title
    }

    fn full_path(&self, _cx: &AppContext) -> std::path::PathBuf {
        self.title.to_path_buf()
    }

    fn file_name<'a>(&'a self, _cx: &'a AppContext) -> &'a std::ffi::OsStr {
        &self.title.as_os_str()
    }

    fn worktree_id(&self) -> usize {
        self.worktree_id
    }

    fn is_deleted(&self) -> bool {
        false
    }

    fn is_created(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self as &dyn Any
    }

    fn to_proto(&self) -> rpc::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }
}

pub fn excerpt_range_over_buffer<M>(
    buffer: &Model<Buffer>,
    cx: &ModelContext<M>,
) -> ExcerptRange<usize> {
    ExcerptRange {
        context: std::ops::Range {
            start: 0 as usize,
            end: buffer.read(cx).len() as _,
        },
        primary: None,
    }
}

pub(crate) fn title_for_cell_excerpt(
    idx: u64,
    cell_id: Option<&String>,
    cell_type: &CellType,
    for_output: bool,
) -> PhonyFile {
    let path_buf: PathBuf = match for_output {
        false => [format!("Cell {idx}"), format!("({:#?})  ", cell_type)]
            .iter()
            .rev()
            .map(|s| s.as_str())
            .collect(),
        true => [format!("[Output — Cell {:#?}]", idx)]
            .iter()
            .map(|s| s.as_str())
            .collect(),
    };
    PhonyFile {
        worktree_id: 0,
        title: Arc::from(path_buf.as_path()),
        cell_idx: CellId::from(idx),
        cell_id: cell_id.map(|id| id.clone()),
    }
}
