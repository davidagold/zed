use gpui::{actions, impl_actions};
use serde::Deserialize;

#[derive(Clone, Deserialize, PartialEq, Default)]
pub enum ToggleNotebookView {
    #[default]
    NotebookEditor,
    Raw,
}

actions!(notebook, [RunCurrentCell, InsertCellAbove, InsertCellBelow]);
