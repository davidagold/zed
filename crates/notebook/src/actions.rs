use gpui::{actions, impl_actions};
use serde::Deserialize;

#[derive(Clone, Deserialize, PartialEq)]
pub enum ToggleNotebookView {
    NotebookEditor,
    Raw,
}

actions!(notebook, [InsertCellAbove, InsertCellBelow, RunCurrentCell]);
impl_actions!(notebook, [ToggleNotebookView]);
