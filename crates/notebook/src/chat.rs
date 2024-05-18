use assistant::{LanguageModel, LanguageModelRequestMessage as Turn};
use gpui;
use gpui::WeakModel;
use itertools::Itertools;
use language::LanguageRegistry;
use project::Project;
use rope::Rope;
use std::sync::Arc;
use ui::ViewContext;

use crate::editor::NotebookEditor;
use crate::Notebook;

pub struct Chat {
    pub(crate) model: LanguageModel,
    pub(crate) messages: Vec<Turn>,
    pub(crate) language_registry: Option<Arc<LanguageRegistry>>,
    system_prompt: Rope,
}

// pub struct Turns(SumTree<CompletionMessage>);

// impl Turns {
//     fn new() -> Self {
//         Self(SumTree::<CompletionMessage>::new())
//     }
// }

impl Chat {
    pub fn new(
        project: WeakModel<Project>,
        model: LanguageModel,
        cx: &mut ViewContext<NotebookEditor>,
    ) -> Self {
        Self {
            model,
            messages: Vec::<Turn>::default(),
            language_registry: project
                .read_with(cx, |project, _cx| project.languages().clone())
                .ok(),
            system_prompt: Rope::new(),
        }
    }

    fn system_prompt(&mut self, notebook: &Notebook) -> Rope {
        let system_prompt = Rope::new();

        system_prompt
    }

    pub fn history(&self) -> Vec<Turn> {
        self.messages.iter().map(Turn::clone).collect_vec()
    }
}
