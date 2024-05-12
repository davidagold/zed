//! Jupyter support for Zed.

use anyhow::anyhow;
use editor::{items::entry_label_color, Editor, EditorEvent, MAX_TAB_TITLE_LEN};
use gpui::{
    AnyView, AppContext, EventEmitter, FocusHandle, FocusableView, Model, ParentElement,
    Subscription, View,
};
use log::{error, info};
use project::{self, Project};
use std::{
    any::{Any, TypeId},
    convert::AsRef,
};
use text::Bias;
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, ViewContext, VisualContext,
};

use util::paths::PathExt;
use workspace::item::{ItemEvent, ItemHandle};

use crate::{
    actions,
    cell::{CellBuilder, CellId},
    do_in,
    kernel::{KernelCommand, KernelEvent},
    Notebook,
};

pub struct NotebookEditor {
    notebook: Model<Notebook>,
    editor: View<Editor>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl NotebookEditor {
    fn new(
        project: Model<Project>,
        notebook_handle: Model<Notebook>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let multi = notebook_handle.read(cx).cells.multi.clone();

        let editor = cx.new_view(|cx| {
            let mut editor = Editor::for_multibuffer(multi, Some(project.clone()), cx);
            editor.set_vertical_scroll_margin(5, cx);
            editor
        });

        let focus_handle = cx.focus_handle();

        cx.on_focus_in(&focus_handle, |this, cx| {
            if this.focus_handle(cx).is_focused(cx) {
                this.editor.focus_handle(cx).focus(cx)
            }
        })
        .detach();

        // TODO: Figure out what goes here.
        // cx.on_focus_out(&focus_handle, |this, cx| {})
        // .detach();
        //
        let mut subscriptions = Vec::<Subscription>::new();

        subscriptions.push(cx.subscribe(&project, |_this, _project, event, _cx| {
            log::info!("Event: {:#?}", event);
        }));
        subscriptions.push(
            cx.subscribe(&editor, |_this, _editor, event: &EditorEvent, cx| {
                cx.emit(event.clone());
                match event {
                    EditorEvent::ScrollPositionChanged { local, autoscroll } => {}
                    _ => {
                        log::info!("Event: {:#?}", event);
                    }
                }
            }),
        );
        if let Some(client_handle) =
            notebook_handle.read_with(cx, |notebook, cx| notebook.client_handle.clone())
        {
            subscriptions.push(cx.subscribe(
                &client_handle,
                |this, client_handle, event: &KernelEvent, cx| {
                    //
                },
            ));
        };

        NotebookEditor {
            notebook: notebook_handle,
            editor,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    fn run_current_cell(&mut self, _: &actions::RunCurrentCell, cx: &mut ViewContext<Self>) -> () {
        let Some((excerpt_id, _, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return ();
        };

        do_in!(|| {
            let notebook = self.notebook.read(cx);
            let current_cell = notebook.cells.get_cell_by_excerpt_id(&excerpt_id)?;
            match notebook
                .client_handle
                .as_ref()?
                .read(cx)
                .run_cell(current_cell, cx)
            {
                Ok(response) => info!("{:#?}", response),
                Err(err) => error!("{:#?}", err),
            };
        });
    }

    fn insert_cell(&mut self, bias: Bias, cx: &mut ViewContext<Self>) -> () {
        let Some((excerpt_id, _, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return ();
        };

        do_in!(|| {
            let notebook = self.notebook.read(cx);
            let mut current_cell_id = notebook.cells.get_cell_by_excerpt_id(&excerpt_id)?.id.get();
            let new_cell_id = match bias {
                Bias::Left => current_cell_id,
                Bias::Right => current_cell_id.pre_inc(),
            };
            let cell = CellBuilder::new(new_cell_id.into()).build();
            self.notebook
                .update(cx, |notebook, cx| notebook.cells.insert(cell, cx));
        });
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
        // self.focus_handle.clone()
        self.editor.focus_handle(cx).clone()
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl ui::prelude::IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.editor.clone())
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
