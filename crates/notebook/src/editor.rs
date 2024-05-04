//! Jupyter support for Zed.

use editor::{
    items::entry_label_color, Editor, EditorEvent, ExcerptRange, MultiBuffer, MAX_TAB_TITLE_LEN,
};
use gpui::{
    impl_actions, AppContext, Context, EventEmitter, FocusHandle, FocusableView, Model,
    ParentElement, View, WeakView,
};
use itertools::Itertools;
use language::{self, Capability};
use project::{self, Project};
use serde_derive::Deserialize;
use std::{any::Any, ops::Range};
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, ViewContext, VisualContext,
};
use util::paths::PathExt;
use workspace::item::ItemEvent;

use crate::{cell::Cell, Notebook};

// #[derive(Default)]
pub struct NotebookEditor {
    active_cell: Option<WeakView<Cell>>,
    notebook: Model<Notebook>,
    // editors: HashMap<CellId, View<Editor>>,
    editor: View<Editor>,
    focus_handle: FocusHandle,
}

impl NotebookEditor {
    fn new(project: Model<Project>, notebook: Model<Notebook>, cx: &mut ViewContext<Self>) -> Self {
        let cell_sources = notebook
            .read(cx)
            .cells
            .iter()
            .map(|cell| cell.source.clone())
            .collect_vec();

        let multi = cx.new_model(|model_cx| {
            let mut multi = MultiBuffer::new(0, Capability::ReadWrite);
            for source in cell_sources {
                let range = ExcerptRange {
                    context: Range {
                        start: 0 as usize,
                        end: source.read(model_cx).len() as usize,
                    },
                    primary: None,
                };
                multi.push_excerpts(source, [range].to_vec(), model_cx);
            }
            multi
        });
        let editor = cx.new_view(|cx| {
            let mut editor = Editor::for_multibuffer(multi, Some(project.clone()), cx);
            editor.set_vertical_scroll_margin(5, cx);
            editor
        });

        NotebookEditor {
            active_cell: None,
            notebook,
            editor,
            focus_handle: cx.focus_handle(),
        }
    }
}

const NOTEBOOK_KIND: &'static str = "NotebookEditor";

impl workspace::item::Item for NotebookEditor {
    type Event = EditorEvent;

    fn to_item_events(event: &Self::Event, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut workspace::Workspace,
        cx: &mut ViewContext<Self>,
    ) {
        self.editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx))
    }

    fn tab_content(
        &self,
        params: workspace::item::TabContentParams,
        cx: &ui::prelude::WindowContext,
    ) -> gpui::AnyElement {
        let title = (&self.notebook).read(cx).file.as_ref().and_then(|f| {
            let path = f.as_local()?.abs_path(cx);
            Some(util::truncate_and_trailoff(
                path.to_str()?,
                MAX_TAB_TITLE_LEN,
            ))
        });

        h_flex()
            .gap_2()
            .when_some(title, |this, title| {
                this.child(
                    Label::new(title)
                        .color(entry_label_color(params.selected))
                        .italic(params.preview),
                )
            })
            .into_any_element()
    }

    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<SharedString> {
        let path = (&self.notebook)
            .read(cx)
            .file
            .as_ref()
            .and_then(|f| f.as_local())?
            .abs_path(cx);

        Some(path.compact().to_string_lossy().to_string().into())
    }

    fn serialized_item_kind() -> Option<&'static str> {
        Some(NOTEBOOK_KIND)
    }
}

impl EventEmitter<EditorEvent> for NotebookEditor {}

impl FocusableView for NotebookEditor {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, cx: &mut ui::prelude::ViewContext<Self>) -> impl ui::prelude::IntoElement {
        let child = self.editor.clone();

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(child)
    }
}

impl workspace::item::ProjectItem for NotebookEditor {
    type Item = Notebook;

    fn for_project_item(
        project: gpui::Model<project::Project>,
        notebook: gpui::Model<Notebook>,
        cx: &mut ui::prelude::ViewContext<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        NotebookEditor::new(project, notebook, cx)
    }
}

// TODO: We'll need to be able to serialize/deserialize the `NotebookEditor` for application restarts.
// impl DeserializeSeed for NotebookEditor {
//     fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
//     where
//         D: serde::Deserializer<'de> {
//             let notebook =
//         }
// }

pub fn init(cx: &mut AppContext) {
    workspace::register_project_item::<NotebookEditor>(cx);
}

#[derive(Clone, Deserialize, PartialEq)]
pub enum ToggleNotebookView {
    NotebookEditor,
    Raw,
}

impl_actions!(workspace, [ToggleNotebookView]);
