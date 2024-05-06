pub mod actions;
pub mod cell;
mod common;
pub mod editor;

use crate::cell::{Cells, KernelSpec};
use crate::common::{forward_err_with, parse_value};
use anyhow::anyhow;
use cell::CellBuilder;
use collections::HashMap;
use gpui::{AsyncAppContext, Context, WeakModel};
use language::Language;
use log::{error, info};
use serde::de::{self, DeserializeSeed, Error, Visitor};
use serde_json::Value;
use std::{io::Read, num::NonZeroU64, sync::Arc};

use project::{self, Project, ProjectPath};
use worktree::File;

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
#[derive(Default)]
pub struct Notebook {
    file: Option<Arc<dyn language::File>>,
    language: Option<Arc<Language>>,
    pub metadata: Option<HashMap<String, Value>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    pub nbformat: usize,
    pub nbformat_minor: usize,
    pub cells: Cells,
}

impl Notebook {
    fn kernel_spec(&self) -> Option<KernelSpec> {
        self.metadata.clone().and_then(|metadata| {
            Some(serde_json::from_value(metadata.get("kernel_spec")?.clone()).ok()?)
        })
    }

    async fn try_set_source_languages<'cx>(
        &mut self,
        project: &WeakModel<Project>,
        cx: &mut AsyncAppContext,
    ) -> anyhow::Result<()> {
        let Some(kernel_spec) = (&self.metadata).as_ref().and_then(|metadata| {
            log::info!("NotebookBuilder.metadata: {:#?}", metadata);
            serde_json::from_value::<KernelSpec>(metadata.get("kernelspec")?.clone()).ok()
        }) else {
            return Err(anyhow::anyhow!("No kernel spec"));
        };

        log::info!("kernel_spec: {:#?}", kernel_spec);

        let cloned_project = project.clone();
        let language = cx
            .spawn(|cx| async move {
                match kernel_spec.language.as_str() {
                    "python" => cloned_project.read_with(&cx, |project, cx| {
                        let languages = project.languages();
                        languages.language_for_name("Python")
                    }),
                    _ => Err(anyhow::anyhow!("Failed to get language")),
                }?
                .await
            })
            .await;

        self.language = language.ok().inspect(|lang| {
            match (|| -> anyhow::Result<()> {
                let handle = &project
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("Cannot upgrade project"))?;

                cx.update_model(handle, |project, cx| {
                    for cell in self.cells.iter() {
                        project.set_language_for_buffer(&cell.source, lang.clone(), cx)
                    }
                })
            })() {
                Ok(_) => log::info!("Successfully set languages for all source buffers"),
                Err(err) => error!(
                    "Failed to set language for at least one source buffer: {:#?}",
                    err
                ),
            }
        });

        Ok(())
    }
}

struct NotebookBuilder<'cx> {
    project_handle: WeakModel<project::Project>,
    file: Option<Arc<dyn language::File>>,
    cx: &'cx mut AsyncAppContext,
    metadata: Option<HashMap<String, serde_json::Value>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    nbformat: Option<usize>,
    nbformat_minor: Option<usize>,
    cells: Cells,
}

impl<'cx> NotebookBuilder<'cx> {
    fn new(
        project_handle: WeakModel<project::Project>,
        file: Option<Arc<dyn language::File>>,
        cx: &'cx mut AsyncAppContext,
    ) -> NotebookBuilder<'cx> {
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

    async fn build(mut self) -> Notebook {
        let mut notebook = Notebook {
            file: self.file,
            language: None,
            metadata: self.metadata,
            nbformat: self.nbformat.unwrap(),
            nbformat_minor: self.nbformat_minor.unwrap(),
            cells: self.cells,
        };

        let _ = notebook
            .try_set_source_languages(&self.project_handle, &mut self.cx)
            .await;

        notebook
    }
}

impl<'cx, 'de: 'cx> Visitor<'de> for NotebookBuilder<'cx> {
    type Value = NotebookBuilder<'cx>;

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
            // Work in a closure to propagate errors without returning early
            let result_parse_entry = (|| -> Result<(), A::Error> {
                match key {
                    "metadata" => self.metadata = parse_value(val)?,
                    "nbformat" => self.nbformat = parse_value(val)?,
                    "nbformat_minor" => self.nbformat_minor = parse_value(val)?,
                    "cells" => {
                        let items: Vec<serde_json::Map<String, serde_json::Value>> =
                            parse_value(val)?;

                        for (idx, item) in items.into_iter().enumerate() {
                            let id = NonZeroU64::new(idx as u64 + 1)
                                .ok_or_else(|| A::Error::custom("Nope"))?
                                .into();

                            let cell_builder =
                                CellBuilder::new(&mut self.project_handle, &mut self.cx, id, item);

                            self.cells.push_cell(cell_builder.build(), &())
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

impl<'cx, 'de: 'cx> DeserializeSeed<'de> for NotebookBuilder<'cx> {
    type Value = NotebookBuilder<'cx>;

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
        // TODO: If the workspace has an active `NotebookEditor` view for the requested `path`,
        //       we should activate the existing view.
        if !path.path.extension().is_some_and(|ext| ext == "ipynb") {
            info!("Failed to detect extension for path `{:#?}`", path);
            return None;
        }
        info!("Detected `.ipynb` extension for path `{:#?}`", path);

        let project = project_handle.downgrade();
        let cloned_path = path.clone();

        let task = app_cx.spawn(|mut cx| async move {
            let buffer_handle = project
                .update(&mut cx, |project, cx| {
                    project.open_buffer(cloned_path.clone(), cx)
                })
                .map_err(|err| anyhow::anyhow!("Failed to open file: {:#?}", err))?
                .await
                .inspect(|_| {
                    info!(
                        "Successfully opened notebook file from path `{:#?}`",
                        cloned_path
                    )
                })?;

            let cloned_project = project.clone();

            let (bytes, file) = buffer_handle
                .read_with(&cx, |buffer, cx| {
                    let mut bytes = Vec::<u8>::with_capacity(buffer.len());
                    let file = (|| {
                        buffer
                            .bytes_in_range(0..buffer.len())
                            .read_to_end(&mut bytes)
                            .inspect(|n_bytes_read| {
                                info!("Successfully read {n_bytes_read} bytes from notebook file")
                            })
                            .ok()?;

                        buffer.file().map(|file| file.clone())
                    })();

                    (bytes, file)
                })
                .map_err(forward_err_with(|err| {
                    format!(
                        "Failed to read notebook from notebook file `{:#?}`: {:#?}",
                        cloned_path, err
                    )
                }))?;

            let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
            let builder = NotebookBuilder::new(cloned_project, file, &mut cx)
                .deserialize(&mut deserializer)
                .map_err(forward_err_with(|err| {
                    format!(
                        "Failed to deserialize notebook from path `{:#?}`: {:#?}",
                        cloned_path, err
                    )
                }))?;

            let notebook = builder.build().await;
            cx.new_model(move |_| notebook)
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
