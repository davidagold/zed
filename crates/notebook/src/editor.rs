//! Jupyter support for Zed.

use editor::{
    items::entry_label_color, Editor, EditorEvent, ExcerptRange, MultiBuffer, MAX_TAB_TITLE_LEN,
};
use gpui::{
    AnyView, AppContext, Context, Entity, EventEmitter, FocusHandle, FocusableView, Model,
    ParentElement, Subscription, View,
};
use itertools::Itertools;
use language::{self, Capability};
use project::{self, Project};
use std::{
    any::{Any, TypeId},
    convert::AsRef,
    ops::Range,
};
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, StyledTypography, ViewContext, VisualContext,
};
use util::paths::PathExt;
use workspace::item::{ItemEvent, ItemHandle};

use crate::{
    actions,
    cell::{Cell, CellId},
    Notebook,
};

pub struct NotebookEditor {
    notebook: Model<Notebook>,
    editor: View<Editor>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
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

        let subscription = cx.subscribe(&project, |this, project, event, cx| {
            log::info!("Event: {:#?}", event);
        });

        cx.subscribe(&editor, |this, _editor, event: &EditorEvent, cx| {
            cx.emit(event.clone());
        })
        .detach();

        let focus_handle = cx.focus_handle();
        // cx.on_focus_in(&focus_handle, |this, cx| {}).detach();

        let this = NotebookEditor {
            notebook,
            editor,
            focus_handle,
            _subscriptions: [subscription].into(),
        };

        this
    }

    fn run_current_cell(&mut self, _: &actions::RunCurrentCell, cx: &mut ViewContext<Self>) {
        let (excerpt_id, buffer_handle, _range) = match self.editor.read(cx).active_excerpt(cx) {
            Some(data) => data,
            None => return,
        };
        let entity_id = cx.entity_id();
        let buffer = buffer_handle.read(cx);
        log::info!(
            "Active cell's `NotebookEditor`'s `EntityId`: {:#?}",
            entity_id
        );
        log::info!("Active cell's `ExcerptId`: {:#?}", excerpt_id);
        log::info!("Active cell's buffer: {:#?}", buffer.text());
    }

    fn toggle_notebook_view(&mut self, cmd: &super::actions::ToggleNotebookView) {}
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
                path.file_name()?.to_string_lossy().as_ref(),
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

    fn for_each_project_item(
        &self,
        cx: &AppContext,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::Item),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a View<Self>,
        cx: &'a AppContext,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }
}

impl EventEmitter<EditorEvent> for NotebookEditor {}

impl FocusableView for NotebookEditor {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
        // self.editor.focus_handle(cx).clone()
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, cx: &mut ui::prelude::ViewContext<Self>) -> impl ui::prelude::IntoElement {
        let child = self.editor.clone();

        div()
            .track_focus(&self.editor.focus_handle(cx))
            .size_full()
            .child(child)
            .on_action(cx.listener(NotebookEditor::run_current_cell))
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

pub fn init(cx: &mut AppContext) {
    workspace::register_project_item::<NotebookEditor>(cx);
}
