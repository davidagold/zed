pub mod actions;
pub mod cell;
mod common;
pub mod editor;
mod jupyter;
mod kernel;

use crate::cell::{Cells, KernelSpec};
use crate::common::{forward_err_with, parse_value};
use crate::jupyter::python::TryAsStr;
use anyhow::{anyhow, Result};
use cell::{Cell, CellBuilder, CellId};
use collections::HashMap;
use gpui::{AsyncAppContext, Context, Model, WeakModel};
use itertools::Itertools;
use kernel::JupyterKernelClient;
use language::Language;
use log::{error, info};
use pyo3::types::PyAnyMethods;
use pyo3::{PyResult, Python};
use serde::de::{self, DeserializeSeed, Visitor};
use serde_json::Value;
use std::{io::Read, sync::Arc};

use project::{self, Project, ProjectPath};
use worktree::File;

// https://nbformat.readthedocs.io/en/latest/format_description.html#top-level-structure
pub struct Notebook {
    file: Option<Arc<dyn language::File>>,
    language: Option<Arc<Language>>,
    pub metadata: Option<HashMap<String, Value>>,
    // TODO: Alias `nbformat` and `nbformat_minor` to include `_version` suffix for clarity
    pub nbformat: usize,
    pub nbformat_minor: usize,
    pub cells: Cells,
    pub client_handle: Option<Model<JupyterKernelClient>>,
}

impl Notebook {
    fn kernel_spec(&self) -> Option<KernelSpec> {
        self.metadata.clone().and_then(|metadata| {
            Some(serde_json::from_value(metadata.get("kernelspec")?.clone()).ok()?)
        })
    }

    async fn try_init_kernel_client(&mut self, cx: &mut AsyncAppContext) -> anyhow::Result<()> {
        self.client_handle
            .replace(JupyterKernelClient::new_model(cx.clone()).await?);
        Ok(())
    }

    pub(crate) async fn try_get_notebook_language(
        &mut self,
        project: &WeakModel<Project>,
        cx: &mut AsyncAppContext,
    ) -> anyhow::Result<()> {
        if self.language.is_none() {
            let Some(kernel_spec) = self.kernel_spec() else {
                return Err(anyhow!("No kernel spec"));
            };
            let cloned_project = project.clone();
            let language = cx
                .spawn(|cx| async move {
                    match kernel_spec.language.as_str() {
                        "python" => cloned_project.read_with(&cx, |project, _cx| {
                            let languages = project.languages();
                            languages.language_for_name("Python")
                        }),
                        _ => Err(anyhow!("Failed to get language")),
                    }?
                    .await
                })
                .await;

            self.language = language.ok();
        };
        Ok(())
    }

    pub(crate) fn try_set_source_languages<C: Context>(
        &mut self,
        cx: &mut C,
        for_cells: Option<Vec<&Cell>>,
    ) -> Vec<CellId> {
        let mut res = Vec::<CellId>::new();
        if self.language.is_none() {
            return res;
        }
        for cell in for_cells.unwrap_or_else(|| self.cells.iter().collect_vec()) {
            cell.source.update(cx, |buffer, cx| {
                buffer.set_language(self.language.clone(), cx);
                res.push(cell.id.get());
            });
        }

        res
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
    cell_builders: Vec<CellBuilder>,
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
            cell_builders: Vec::<CellBuilder>::default(),
        }
    }

    async fn build(mut self) -> Result<Notebook> {
        let mut notebook = Notebook {
            file: self.file,
            language: None,
            metadata: self.metadata,
            nbformat: self.nbformat.unwrap(),
            nbformat_minor: self.nbformat_minor.unwrap(),
            cells: Cells::from_builders(self.cell_builders, self.cx)?,
            client_handle: None,
        };

        let _ = notebook
            .try_get_notebook_language(&self.project_handle, &mut self.cx)
            .await;
        let _ = do_in!(|| self.cx.update_model(
            &self.project_handle.upgrade()?,
            |_project_handle, cx| {
                notebook.try_set_source_languages(cx, None);
            }
        ));

        Ok(notebook)
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
            let result_parse_entry = do_in!(|| -> Result<(), A::Error> {
                match key {
                    "metadata" => self.metadata = parse_value(val)?,
                    "nbformat" => self.nbformat = parse_value(val)?,
                    "nbformat_minor" => self.nbformat_minor = parse_value(val)?,
                    "cells" => {
                        let items: Vec<serde_json::Map<String, serde_json::Value>> =
                            parse_value(val)?;

                        for (id, item) in items.into_iter().enumerate() {
                            let cell_builder = CellBuilder::new((id + 1) as _).process_map(
                                item,
                                &self.project_handle,
                                &mut self.cx,
                            );

                            self.cell_builders.push(cell_builder)
                        }
                    }
                    _ => {}
                };

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
            return None;
        }

        let project = project_handle.downgrade();
        let cloned_path = path.clone();

        let task = app_cx.spawn(|mut cx| async move {
            let buffer_handle = project
                .update(&mut cx, |project, cx| {
                    project.open_buffer(cloned_path.clone(), cx)
                })
                .map_err(|err| anyhow!("Failed to open file: {:#?}", err))?
                .await?;

            let cloned_project = project.clone();

            let (bytes, file) = buffer_handle
                .read_with(&cx, |buffer, cx| {
                    let mut bytes = Vec::<u8>::with_capacity(buffer.len());
                    let file = do_in!(|| -> Option<Arc<dyn language::File>> {
                        buffer
                            .bytes_in_range(0..buffer.len())
                            .read_to_end(&mut bytes)
                            .inspect(|n_bytes_read| {
                                info!("Successfully read {n_bytes_read} bytes from notebook file")
                            })
                            .ok()?;

                        buffer.file().map(|file| file.clone())
                    });

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

            let mut notebook = builder.build().await?;

            pyo3::prepare_freethreaded_python();
            if let Err(err) = do_in!(|py| -> PyResult<_> {
                let sys = py.import_bound("sys")?;
                let version = sys.getattr("version")?;

                let kc_module_dir = std::env::current_dir()?.join("crates/notebook/src/jupyter");
                sys.getattr("path")?
                    .call_method1("insert", (0, kc_module_dir.to_str()))?;
                do_in!(|| {
                    info!("Found Python version: {}", version.__str__()?);
                    let exec = sys.getattr("executable").ok()?;
                    info!("Python executable: {}", exec.__str__()?);
                });

                let pythonpath = sys.getattr("path")?.extract::<Vec<String>>()?;
                do_in!(|| {
                    let str_python_path = serde_json::to_string_pretty(&pythonpath).ok()?;
                    info!("Found Python path: {str_python_path}");
                });

                Ok(())
            }) {
                error!("Failed to initialize Python process: {:#?}", err);
                return Err(err.into());
            };
            notebook
                .try_init_kernel_client(&mut cx)
                .await
                .map_err(forward_err_with(|err: anyhow::Error| err.to_string()))?;

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
