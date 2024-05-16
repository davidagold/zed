use crate::jupyter::message::{IoPubSubMessageType, MessageType};
use crate::{do_in, jupyter::message::Message};
use anyhow::{anyhow, Result};
use collections::HashMap;
use editor::{ExcerptId, ExcerptRange, MultiBuffer};
use gpui::{AppContext, AsyncAppContext, Flatten, Model, ModelContext, WeakModel};
use itertools::Itertools;
use language::{Buffer, Capability, File};
use project::Project;
use rope::{Rope, TextSummary};
use runtimelib::media::MimeType;
use serde::{de::Visitor, Deserialize};
use serde_json::Value;
use std::cell::Cell as StdCell;
use std::{any::Any, fmt::Debug, path::PathBuf, sync::Arc};
use sum_tree::{Cursor, Dimension, SumTree, Summary};
use text::Bias;
use ui::Context;

#[derive(Clone, Debug)]
pub struct Cell {
    pub id: StdCell<CellId>,
    // The `msg_id` of the latest `execute_request` message sent to request the execution of the present cell
    pub(crate) latest_execution_request_msg_id: Option<String>,
    pub(crate) text_summary: TextSummary,

    // `cell_id` is a notebook field, whereas `cell.id` is the `KeyedItem` associated type  for a `SumTree<Cell>`
    pub cell_id: Option<String>,
    pub cell_type: CellType,
    pub metadata: HashMap<String, serde_json::Value>,
    pub source: Model<Buffer>,
    pub execution_count: Option<usize>,
    pub outputs: Option<Vec<IpynbCodeOutput>>,
    pub output_content: Option<Model<Buffer>>,
}

impl Cell {
    pub(crate) fn update_titles<C: Context>(&self, cx: &mut C) {
        cx.update_model(&self.source, |buffer, cx| {
            let title = title_for_cell_excerpt(
                self.id.get().into(),
                self.cell_id.as_ref(),
                &self.cell_type,
                false,
            );
            buffer.file_updated(Arc::new(title), cx);
        });
        do_in!(
            || cx.update_model(self.output_content.as_ref()?, |buffer, cx| {
                let title = title_for_cell_excerpt(
                    self.id.get().into(),
                    self.cell_id.as_ref(),
                    &self.cell_type,
                    true,
                );
                buffer.file_updated(Arc::new(title), cx);
            })
        );
    }

    fn increment_id<C: Context>(&self, cx: &mut C) -> &Self {
        let incremented_id = self.id.get().pre_inc();
        self.id.set(incremented_id);
        self.update_titles(cx);
        self
    }

    pub(crate) fn update_text_summary<C: Context>(&mut self, cx: &C) {
        // let mut buffer_handles = vec![&self.source];
        self.source.read_with(cx, |buffer, cx| {
            self.text_summary = buffer.text_snapshot().text_summary();
        });
        if let Some(buffer_handle) = &self.output_content {
            buffer_handle.read_with(cx, |buffer, cx| {
                self.text_summary += buffer.text_summary();
                self.text_summary.lines.row += 1;
            });
        };
    }

    pub(crate) fn buffer_handles(&self) -> Vec<&Model<Buffer>> {
        let mut buffer_handles = vec![&self.source];
        do_in!(|| buffer_handles.push(self.output_content.as_ref()?));
        buffer_handles
    }
}

#[derive(Default)]
pub struct CellBuilder {
    id: u64,
    text_summary: Option<TextSummary>,
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

    pub fn source(mut self, source: Model<Buffer>) -> Self {
        self.source.replace(source);
        self
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

                        let mut source_text = source_lines.join("\n");
                        source_text.push_str("\n");
                        self.text_summary = Some(TextSummary::from(source_text.as_str()));

                        project_handle
                            .update(cx, |_project, cx| -> Result<()> {
                                let source_buffer =
                                    cx.new_model(|cx| Buffer::local(source_text, cx));
                                self.source.replace(source_buffer);
                                Ok(())
                            })
                            .flatten()?;
                    }
                    "execution_count" => {
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
                        };
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

    pub fn build<C: Context>(self, cx: &mut C) -> Cell {
        let mut cell = Cell {
            id: CellId(self.id).into(),
            latest_execution_request_msg_id: None,
            text_summary: self.text_summary.unwrap_or_else(|| TextSummary::from("")),
            cell_id: self.cell_id,
            cell_type: self.cell_type.unwrap_or_default(),
            metadata: self.metadata.unwrap_or_default(),
            source: self.source.unwrap(),
            execution_count: self.execution_count,
            outputs: self.outputs,
            output_content: self.output_content,
        };
        cell.update_text_summary(cx);
        cell
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

pub(crate) fn insert_as_excerpts_into_multibuffer(
    buffer_handles: impl IntoIterator<Item = Model<Buffer>>,
    prev_excerpt_id: u64,
    multi: &mut MultiBuffer,
    cx: &mut ModelContext<MultiBuffer>,
) -> Result<Option<ExcerptId>> {
    let mut id_last_excerpt_added: Option<ExcerptId> = None;
    buffer_handles.into_iter().for_each(|handle| {
        let span = ExcerptRange {
            context: std::ops::Range {
                start: 0 as usize,
                end: cx.read_model(&handle, |buffer, cx| buffer.len() as usize),
            },
            primary: None,
        };
        let prev_excerpt_id =
            id_last_excerpt_added.unwrap_or(ExcerptId::from_proto(prev_excerpt_id));
        let ids_added = multi.insert_excerpts_after(prev_excerpt_id, handle, vec![span], cx);
        do_in!(|| {
            id_last_excerpt_added.replace(*ids_added.last()?);
        });
    });
    Ok(id_last_excerpt_added)
}

pub(crate) fn excerpt_ids_for_cell(
    multi: &MultiBuffer,
    cell: &Cell,
    cx: &AppContext,
) -> Vec<ExcerptId> {
    let mut excerpt_ids = Vec::<ExcerptId>::new();
    let mut buffers = vec![&cell.source];
    do_in!(|| buffers.push(cell.output_content.as_ref()?));
    for buffer in buffers {
        excerpt_ids.extend(
            multi
                .excerpts_for_buffer(buffer, cx)
                .into_iter()
                .map(|(id, _)| id)
                .collect_vec(),
        );
    }
    excerpt_ids
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

    pub fn get_cell_by_id(&self, cell_id: &CellId) -> Option<&Cell> {
        let mut cursor = self.cursor::<CellId>();
        cursor.seek_forward(cell_id, text::Bias::Left, &());
        cursor.item()
    }

    pub fn get_cell_by_buffer<C: Context>(
        &self,
        buffer_handle: &Model<Buffer>,
        cx: &C,
    ) -> Option<&Cell> {
        let mut res: Option<&Cell> = None;
        buffer_handle.read_with(cx, |buffer, cx| {
            let target_buffer_id = buffer.remote_id();
            for cell in self.tree.iter() {
                cell.source.read_with(cx, |buffer, _cx| {
                    if target_buffer_id == buffer.remote_id() {
                        res.replace(cell);
                    };
                })
            }
        });
        res
    }

    pub(crate) fn insert<C: Context>(
        &mut self,
        iterable_cells: impl IntoIterator<Item = Cell>,
        cx: &mut C,
    ) {
        self.multi.update(cx, |multi, cx| {
            let cells_to_insert = SumTree::<Cell>::from_iter(iterable_cells.into_iter(), &());
            if cells_to_insert.first().is_none() {
                return Err(anyhow!("No cells to insert"));
            };
            for cell in cells_to_insert.iter() {
                cell.update_titles(cx);
            }
            let id_first_to_insert = cells_to_insert.first().unwrap().id.get();

            let mut cursor = self.tree.cursor::<CellId>();
            let mut new_tree = cursor.slice(&id_first_to_insert, Bias::Left, &());
            let prev_cell = cursor.prev_item().ok_or_else(|| anyhow!("Nope"));
            let mut prev_excerpt_id = prev_cell
                .ok()
                .and_then(|prev_cell| {
                    excerpt_ids_for_cell(multi, prev_cell, cx)
                        .into_iter()
                        .last()
                    // .into_iter()
                    // .map(|(id, _)| id)
                    // .last()
                })
                .unwrap_or(ExcerptId::min());

            let (shifted_cells, ids_excerpts_to_remove) = cursor.fold(
                (Vec::<Cell>::new(), Vec::<ExcerptId>::new()),
                |(mut cells, mut excerpt_ids), cell| {
                    excerpt_ids.extend(excerpt_ids_for_cell(multi, &cell, cx));
                    cells.push(cell.increment_id(cx).clone());
                    (cells, excerpt_ids)
                },
            );

            for cell in cells_to_insert.iter().chain(shifted_cells.iter()) {
                new_tree.insert_or_replace(cell.clone(), &());
                let mut buffers_to_insert = vec![cell.source.clone()];
                do_in!(|| buffers_to_insert.push(cell.output_content.as_ref()?.clone()));
                match insert_as_excerpts_into_multibuffer(
                    buffers_to_insert,
                    prev_excerpt_id.to_proto(),
                    multi,
                    cx,
                ) {
                    Ok(id_last_excerpt_added) => {
                        do_in!(|| prev_excerpt_id = id_last_excerpt_added?)
                    }
                    Err(err) => return Err(err),
                };
            }
            multi.remove_excerpts(ids_excerpts_to_remove, cx);
            self.tree = new_tree;
            Ok(())
        });
    }

    pub(crate) fn update_cell_from_msg<C: Context>(
        &mut self,
        cell_id: &CellId,
        mut msg: Message,
        cx: &mut C,
    ) -> Result<()>
    where
        C::Result<Model<Buffer>>: Into<Model<Buffer>>,
    {
        let Some(cell) = self.get_cell_by_id(cell_id) else {
            return Err(anyhow!("No cell with cell ID {:#?}", cell_id));
        };

        let buffer_handle = match cell.output_content.as_ref() {
            Some(buffer_handle) => buffer_handle.clone(),
            None => cx.new_model(|cx| Buffer::local("", cx)).into(),
        };

        let mut text: Option<String> = None;
        match &msg.msg_type {
            MessageType::IoPubSub(io_pubsub_msg_type) => match io_pubsub_msg_type {
                IoPubSubMessageType::Stream => {
                    do_in!(|| {
                        text.replace(serde_json::from_value(msg.content.remove("text")?).ok()?);
                    });
                }
                IoPubSubMessageType::ExecutionError => {
                    do_in!(|| {
                        let error_name: String =
                            serde_json::from_value(msg.content.remove("ename")?).ok()?;
                        let error_value: String =
                            serde_json::from_value(msg.content.remove("evalue")?).ok()?;
                        let traceback: Vec<String> =
                            serde_json::from_value(msg.content.remove("traceback")?).ok()?;

                        text.replace(format!(
                            "{}: {}\n{}",
                            error_name,
                            error_value,
                            traceback.join("\n")
                        ));
                    });
                }
                // TODO: Remaining PubSub message types
                _ => {}
            },
            // TODO: Remaining message types where applicable
            _ => {}
        };

        if text.is_none() {
            return Ok(());
        }

        let mut updated = cell.clone();
        updated.output_content.replace(buffer_handle.clone());
        updated.update_titles(cx); // This changes the file of `buffer_handle`, which updates the tab title, so we update before inserting
        updated.update_text_summary(cx);
        if cell.output_content.is_none() {
            self.multi.update(cx, |multi, cx| {
                do_in!(|| insert_as_excerpts_into_multibuffer(
                    vec![buffer_handle.clone()],
                    excerpt_ids_for_cell(multi, &cell, cx).last()?.to_proto(),
                    multi,
                    cx,
                ));
            });
        }
        buffer_handle.update(cx, |buffer, cx| Some(buffer.set_text(text?, cx)?));

        self.tree.insert_or_replace(updated, &());
        Ok(())
    }

    pub fn from_builders<'c>(
        builders: Vec<CellBuilder>,
        cx: &mut AsyncAppContext,
    ) -> Result<Cells> {
        let tree = SumTree::<Cell>::new();
        let multi = cx.new_model(|_cx| MultiBuffer::new(0, Capability::ReadWrite))?;
        let mut this = Cells { tree, multi };
        let cells_to_insert = builders.into_iter().map(|b| b.build(cx)).collect_vec();
        this.insert(cells_to_insert, cx);
        Ok(this)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CellSummary {
    num_cells_inclusive: u64,
    pub(crate) text_summary: TextSummary,
}

impl Summary for CellSummary {
    type Context = ();

    fn add_summary(&mut self, summary: &Self, _cx: &Self::Context) {
        self.num_cells_inclusive += summary.num_cells_inclusive;
        self.text_summary += &summary.text_summary;
    }
}

impl sum_tree::Item for Cell {
    type Summary = CellSummary;

    fn summary(&self) -> Self::Summary {
        // do_in!(|| buffer_handles.push(self.output_content.as_ref()?));
        let mut text_summary = self.text_summary.clone();
        text_summary.lines.row += 1;
        CellSummary {
            num_cells_inclusive: 1,
            text_summary,
        }
    }
}

impl sum_tree::KeyedItem for Cell {
    type Key = CellId;

    fn key(&self) -> Self::Key {
        self.id.get()
    }
}

impl<'a> Dimension<'a, CellSummary> for CellId {
    fn add_summary(&mut self, summary: &'a CellSummary, _: &<CellSummary as Summary>::Context) {
        self.0 = self.0 + summary.num_cells_inclusive
    }
    fn from_summary(summary: &'a CellSummary, cx: &<CellSummary as Summary>::Context) -> Self {
        CellId::from(summary.num_cells_inclusive)
    }
}

impl<'a> Dimension<'a, CellSummary> for TextSummary {
    fn add_summary(&mut self, summary: &'a CellSummary, _: &<CellSummary as Summary>::Context) {
        *self += &summary.text_summary;
    }
    fn from_summary(summary: &'a CellSummary, cx: &<CellSummary as Summary>::Context) -> Self {
        summary.text_summary.clone()
    }
}

impl<'a> sum_tree::SeekTarget<'a, CellSummary, CellSummary> for CellId {
    fn cmp(
        &self,
        cursor_location: &CellSummary,
        cx: &<CellSummary as Summary>::Context,
    ) -> std::cmp::Ordering {
        Ord::cmp(&(*self).into(), &cursor_location.num_cells_inclusive)
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

impl From<CellId> for u64 {
    fn from(id: CellId) -> Self {
        id.0
    }
}

impl CellId {
    pub(crate) fn min() -> CellId {
        CellId(0)
    }

    pub(crate) fn pre_inc(&mut self) -> Self {
        self.0 += 1;
        *self
    }

    pub(crate) fn post_inc(&mut self) -> Self {
        let id = *self;
        self.0 += 1;
        id
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

// `DisplayMapping` (`ViewMatter` ... or `IntoView<Of = V>)
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

// TODO: Just pass the cell
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