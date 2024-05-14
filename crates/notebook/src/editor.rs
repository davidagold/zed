//! Jupyter support for Zed.

use anyhow::anyhow;
use editor::{items::entry_label_color, Editor, EditorEvent, MAX_TAB_TITLE_LEN};
use gpui::{
    AnyView, AppContext, EventEmitter, FocusHandle, FocusableView, Model, ParentElement,
    Subscription, View,
};
use itertools::Itertools;
use language::Buffer;
use log::{error, warn};
use project::{self, Project};
use std::{
    any::{Any, TypeId},
    convert::AsRef,
};
use text::Bias;
use ui::{
    div, h_flex, Context, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon,
    Render, SharedString, Styled, ViewContext, VisualContext,
};

use util::paths::PathExt;
use workspace::item::{ItemEvent, ItemHandle};

use crate::{
    actions,
    cell::{Cell, CellBuilder, CellId, CellType},
    do_in,
    kernel::{JupyterKernelClient, KernelEvent},
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
                    EditorEvent::TitleChanged => {
                        cx.notify();
                    }
                    _ => {}
                }
            }),
        );
        if let Some(client_handle) =
            notebook_handle.read_with(cx, |notebook, _cx| notebook.client_handle.clone())
        {
            subscriptions.push(cx.subscribe(
                &client_handle,
                |this, _client_handle, event: &KernelEvent, cx| match event.clone() {
                    KernelEvent::ReceivedKernelMessage { msg, cell_id } => {
                        this.notebook.update(cx, |notebook, cx| {
                            notebook.cells.update_cell_from_msg(&cell_id, msg, cx);
                        })
                    }
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
        let Some((_, buffer_handle, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return ();
        };
        if let Err(err) = self
            .notebook
            .read_with(cx, |notebook, cx| {
                do_in!(|| -> Option<(Cell, &JupyterKernelClient)> {
                    let current_cell = notebook
                        .cells
                        .get_cell_by_buffer(&buffer_handle, cx)?
                        .clone();
                    let client_handle = notebook.client_handle.as_ref()?.read(cx);
                    Some((current_cell, client_handle))
                })
                .ok_or_else(|| anyhow!("Failed to get current cell or client handle"))
                .and_then(|(current_cell, client_handle)| {
                    let response = client_handle.run_cell(&current_cell, cx)?;
                    anyhow::Ok((current_cell, response))
                })
            })
            .and_then(|(mut current_cell, _response)| {
                if current_cell.output_content.is_some() {
                    current_cell.output_content = None;
                    self.notebook.update(cx, |notebook, cx| {
                        notebook
                            .cells
                            // TODO: Update execution count and so on
                            .try_replace_with(cx, &current_cell.id.get(), |_cell| Ok(current_cell))
                    })?;
                };
                Ok(())
            })
        {
            error!("{:#?}", err);
        }
    }

    fn run_current_selection(
        &mut self,
        _: &actions::RunCurrentSelection,
        cx: &mut ViewContext<Self>,
    ) -> () {
        self.notebook.update(cx, |notebook, cx| {
            let cells = &mut notebook.cells;
            do_in!(|| {
                let editor = self.editor.read(cx);
                let (_, buffer_handle, _) = editor.active_excerpt(cx)?;
                let mut next_cell_id = cells.get_cell_by_buffer(&buffer_handle, cx)?.id.get();
                let (_, range, _) = cells
                    .multi
                    .read_with(cx, |multi, cx| {
                        let range_in_multi = editor.selections.newest::<usize>(cx).range();
                        multi.range_to_buffer_ranges(range_in_multi, cx)
                    })
                    .into_iter()
                    .at_most_one()
                    .ok()??;
                let snapshot = buffer_handle.read(cx).text_snapshot();
                let mut start = 0;
                for (end, replace) in vec![
                    (range.start, false),
                    (range.end, true),
                    (snapshot.len(), false),
                ] {
                    let cell = CellBuilder::new(next_cell_id.post_inc().clone().into())
                        .source(cx.new_model(|cx| {
                            let mut buffer = Buffer::local(
                                snapshot.text_for_range(start..end).collect::<String>(),
                                cx,
                            );
                            buffer.set_language(notebook.language.clone(), cx);
                            buffer
                        }))
                        .build();
                    if end == range.end {
                        notebook
                            .client_handle
                            .as_ref()?
                            .read_with(cx, |client, cx| {
                                client.run_cell(&cell, cx);
                            });
                    };
                    cells.insert(vec![cell], cx, replace);
                    start = end;
                }
            });
        });

        // let buffer = editor.buffer().read(cx);
    }

    fn insert_cell_above(
        &mut self,
        _cmd: &actions::InsertCellAbove,
        cx: &mut ViewContext<Self>,
    ) -> () {
        self.insert_cell(Bias::Left, cx)
    }

    fn insert_cell_below(
        &mut self,
        _cmd: &actions::InsertCellBelow,
        cx: &mut ViewContext<Self>,
    ) -> () {
        self.insert_cell(Bias::Right, cx)
    }

    fn insert_cell(&mut self, bias: Bias, cx: &mut ViewContext<Self>) -> () {
        let Some((_, buffer_handle, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return ();
        };

        do_in!(|| {
            let mut current_cell_id = self.notebook.read_with(cx, |notebook, cx| {
                let current_cell = notebook.cells.get_cell_by_buffer(&buffer_handle, cx)?;
                Some(current_cell.id.get().clone())
            })?;
            let new_cell_id = match bias {
                Bias::Left => current_cell_id,
                Bias::Right => current_cell_id.pre_inc(),
            };
            let source = cx.new_model(|cx| Buffer::local("", cx));
            let cell = CellBuilder::new(new_cell_id.into()).source(source).build();
            let _ = self.notebook.update(cx, |notebook, cx| {
                match cell.cell_type {
                    CellType::Code => {
                        notebook.try_set_source_languages(cx, Some(vec![&cell]));
                    }
                    _ => {}
                }
                notebook.cells.insert(vec![cell], cx, false);
            });
        });
    }
}

const NOTEBOOK_KIND: &'static str = "NotebookEditor";

impl workspace::item::Item for NotebookEditor {
    type Event = EditorEvent;

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
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
        _cx: &'a AppContext,
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
            .on_action(cx.listener(NotebookEditor::insert_cell_above))
            .on_action(cx.listener(NotebookEditor::insert_cell_below))
            .on_action(cx.listener(NotebookEditor::run_current_selection))
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
