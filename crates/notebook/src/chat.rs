use assistant::completion_provider::CompletionProvider;
use assistant::{LanguageModel, LanguageModelRequest, LanguageModelRequestMessage as Turn, Role};
use futures::StreamExt;
use gpui::{self, AppContext, AsyncWindowContext, Result, Task, WeakView};
use gpui::{Model, WeakModel};
use itertools::Itertools;
use language::LanguageRegistry;
use project::Project;
use std::sync::Arc;
use ui::{Context, ViewContext};

use crate::editor::NotebookEditor;
use crate::{do_in, Notebook};

pub struct Chat {
    pub(crate) model: LanguageModel,
    pub(crate) messages: Vec<Turn>,
    pub(crate) language_registry: Option<Arc<LanguageRegistry>>,
}

impl Chat {
    pub fn new(
        project: WeakModel<Project>,
        model: LanguageModel,
        notebook_handle: &Model<Notebook>,
        cx: &mut ViewContext<NotebookEditor>,
    ) -> Self {
        let mut chat = Self {
            model,
            messages: Vec::<Turn>::default(),
            language_registry: project
                .read_with(cx, |project, _cx| project.languages().clone())
                .ok(),
        };
        cx.read_model(notebook_handle, |notebook, cx| {
            chat.set_system_prompt(notebook, cx);
        });
        chat
    }

    fn set_system_prompt(&mut self, notebook: &Notebook, cx: &AppContext) {
        let system_prompt = vec![
            "You are a coding and research assistant specializing in the analysis of interactive notebooks.",
            "A notebook state is included below.",
            "Each cell is denoted by a code block including the source code of the cell and a leading comment indicating the ID of the cell.",
            ""
        ].join("\n");

        let cells_paragraphs = notebook
            .cells
            .iter()
            .map(|cell| {
                let mut paragraph = vec![
                    "```".into(),
                    cell.source.read(cx).text_snapshot().text(),
                    "```".into(),
                ];

                do_in!(|| {
                    let language = cell.source.read(cx).language()?;
                    paragraph.get_mut(0)?.push_str(language.name().as_ref());
                    let scope = language.default_scope();
                    let (delim_open, delim_close) = scope.block_comment_delimiters()?;
                    let cell_id_comment = [
                        delim_open.to_string(),
                        format!("Source code for Cell {}", u64::from(cell.id.get())),
                        delim_close.to_string(),
                    ]
                    .join("\n");
                    paragraph.insert(1, cell_id_comment);
                });
                paragraph.join("\n")
            })
            .join("\n\n");

        let system_prompt = Turn {
            role: Role::System,
            content: vec![system_prompt, cells_paragraphs].join("\n\n"),
        };
        self.messages.insert(0, system_prompt);
    }

    pub fn next_turn<F, V: 'static>(
        &mut self,
        cx: &mut ViewContext<'_, V>,
        mut on_delta: F,
    ) -> Task<Result<()>>
    where
        F: FnMut(&mut WeakView<V>, String, &mut AsyncWindowContext) -> Result<()> + 'static,
    {
        let request = LanguageModelRequest {
            model: self.model.clone(),
            messages: self.history(),
            stop: vec![],
            temperature: 1.0,
        };
        let stream = CompletionProvider::global(cx).complete(request);
        cx.spawn(|mut view, mut cx| async move {
            let mut deltas = stream.await?;
            while let Some(Ok(delta)) = deltas.next().await {
                on_delta(&mut view, delta, &mut cx)?;
                smol::future::yield_now().await;
            }
            anyhow::Ok(())
        })
    }

    pub fn history(&self) -> Vec<Turn> {
        self.messages.iter().map(Turn::clone).collect_vec()
    }
}
