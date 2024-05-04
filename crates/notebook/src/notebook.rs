pub mod actions;
pub mod cell;
mod common;
pub mod editor;

use crate::cell::{CellId, Cells};
use crate::common::parse_value;
use cell::CellBuilder;
use collections::HashMap;
use gpui::{Context, ModelContext, WeakModel};
use log::{error, info};
use serde::de::{self, DeserializeSeed, Error, Visitor};
use std::{io::Read, num::NonZeroU64, sync::Arc};

use project::{self, ProjectPath};
use worktree::File;

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
#[derive(Default)]
pub struct Notebook {
    file: Option<Arc<dyn language::File>>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: usize,
    nbformat_minor: usize,
    cells: Cells,
}

struct NotebookBuilder<'nbb, 'cx: 'nbb> {
    project_handle: WeakModel<project::Project>,
    file: Option<Arc<dyn language::File>>,
    cx: &'nbb mut ModelContext<'cx, Notebook>,
    metadata: Option<HashMap<String, serde_json::Value>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: Option<usize>,
    nbformat_minor: Option<usize>,
    cells: Cells,
}

impl<'nbb, 'cx: 'nbb> NotebookBuilder<'nbb, 'cx> {
    fn new(
        project_handle: WeakModel<project::Project>,
        file: Option<Arc<dyn language::File>>,
        cx: &'nbb mut ModelContext<'cx, Notebook>,
    ) -> NotebookBuilder<'nbb, 'cx> {
        NotebookBuilder {
            project_handle,
            file,
            cx,
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

impl<'nbb, 'cx, 'de> Visitor<'de> for NotebookBuilder<'nbb, 'cx>
where
    'cx: 'nbb,
    'nbb: 'de,
{
    type Value = NotebookBuilder<'nbb, 'cx>;

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
            let result_parse_entry = (|| -> Result<(), A::Error> {
                match key {
                    "metadata" => self.metadata = parse_value(val)?,
                    "nbformat" => self.nbformat = parse_value(val)?,
                    "nbformat_minor" => self.nbformat_minor = parse_value(val)?,
                    "cells" => {
                        let items: Vec<serde_json::Map<String, serde_json::Value>> =
                            parse_value(val)?;

                        for (idx, item) in items.into_iter().enumerate() {
                            let id: CellId = NonZeroU64::new(idx as u64 + 1)
                                .ok_or_else(|| A::Error::custom("Nope"))?
                                .into();

                            let cell_builder =
                                CellBuilder::new(&mut self.project_handle, &mut self.cx, id, item);

                            self.cells.push_cell(cell_builder.build(), &());
                        }
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
        Ok(self)
    }
}

impl<'nbb, 'cx, 'de> DeserializeSeed<'de> for NotebookBuilder<'nbb, 'cx>
where
    'cx: 'nbb,
    'nbb: 'de,
{
    type Value = NotebookBuilder<'nbb, 'cx>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
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
            info!("Failed to detect extension for path `{:#?}`", path);
            return None;
        }
        info!("Detected `.ipynb` extension for path `{:#?}`", path);

        let project = project_handle.downgrade();
        let open_buffer_task =
            project.update(app_cx, |project, cx| project.open_buffer(path.clone(), cx));

        let task = app_cx.spawn(|mut cx| async move {
            let buffer_handle = open_buffer_task?.await?;

            info!("Successfully opened buffer");

            cx.new_model(move |cx_model| {
                let buffer = buffer_handle.read(cx_model);
                let mut bytes = Vec::<u8>::with_capacity(buffer.len());

                let n_bytes_maybe_read = buffer
                    .bytes_in_range(0..buffer.len())
                    .read_to_end(&mut bytes);

                match n_bytes_maybe_read {
                    Ok(n_bytes) => info!("Successfully read {} bytes from notebook file", n_bytes),
                    Err(err) => error!("Failed to read from notebook file: {:#?}", err),
                }

                let file = buffer.file().map(|file| file.clone());

                let mut deserializer = serde_json::Deserializer::from_slice(&bytes);

                NotebookBuilder::new(project, file, cx_model)
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
