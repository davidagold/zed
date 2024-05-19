//! Jupyter support for Zed.

use anyhow::{anyhow, Result};
use assistant::{
    completion_provider::CompletionProvider, LanguageModel, LanguageModelRequest,
    LanguageModelRequestMessage,
};
use editor::{
    items::entry_label_color, scroll::Autoscroll, Editor, EditorEvent, MultiBuffer,
    MAX_TAB_TITLE_LEN,
};
use futures::StreamExt;
use gpui::{
    AnyView, AppContext, AsyncAppContext, AsyncWindowContext, EventEmitter, FocusHandle,
    FocusableView, Model, ModelContext, ParentElement, Subscription, View, WeakView,
};
use itertools::Itertools;
use language::Buffer;
use log::{error, info};
use project::{self, Project};
use rope::{Point, TextSummary};
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
    actions::{self, NewChatCell},
    cell::{self, Cell, CellBuilder, CellId, CellType, Cells},
    chat::Chat,
    common::UpdateInner,
    do_in,
    kernel::KernelEvent,
    Notebook,
};

pub struct NotebookEditor {
    pub(crate) notebook: Model<Notebook>,
    editor: View<Editor>,
    focus_handle: FocusHandle,
    chat: Chat,
    _subscriptions: Vec<Subscription>,
}

impl UpdateInner<Notebook> for WeakView<NotebookEditor> {
    type OuterContext = AsyncWindowContext;
    type InnerContext<'cx, T> = ModelContext<'cx, T>;

    fn update_inner<'inner, 'outer: 'inner, F, R>(
        &self,
        cx: &'outer mut AsyncWindowContext,
        update: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Notebook, &mut ModelContext<'_, Notebook>) -> R,
    {
        self.update(cx, |nb_editor, cx| nb_editor.notebook.update(cx, update))
    }
}

impl UpdateInner<MultiBuffer> for WeakView<NotebookEditor> {
    type OuterContext = AsyncWindowContext;
    type InnerContext<'cx, T> = ModelContext<'cx, T>;

    fn update_inner<'inner, 'outer: 'inner, F, R>(
        &self,
        cx: &'outer mut AsyncWindowContext,
        update: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut MultiBuffer, &mut ModelContext<'_, MultiBuffer>) -> R,
    {
        self.update_inner(cx, |notebook: &mut Notebook, cx| {
            notebook.cells.multi.update(cx, update)
        })
    }
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

        let mut subscriptions = Vec::<Subscription>::new();

        subscriptions.push(cx.subscribe(&project, |_this, _project, event, _cx| {
            log::info!("Event: {:#?}", event);
        }));
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

        let chat = Chat::new(
            project.downgrade(),
            LanguageModel::OpenAi(open_ai::Model::FourTurbo),
            &notebook_handle,
            cx,
        );
        NotebookEditor {
            notebook: notebook_handle,
            editor,
            focus_handle,
            chat: chat,
            _subscriptions: subscriptions,
        }
    }

    pub fn active_cell<'cell, 'cx: 'cell>(&self, cx: &'cx AppContext) -> Option<&'cell Cell> {
        let Some((_, buffer_handle, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return None;
        };
        self.notebook
            .read(cx)
            .cells
            .get_cell_by_buffer(&buffer_handle, cx)
    }

    fn run_current_cell(&mut self, _: &actions::RunCurrentCell, cx: &mut ViewContext<Self>) -> () {
        do_in!(|| {
            let current_cell = self.active_cell(cx)?.clone();
            let next_cell_id = current_cell.id.get().pre_inc();
            self.focus_cell(&next_cell_id, None, cx);
            match current_cell.cell_type {
                CellType::Code => {
                    self.notebook
                        .read(cx)
                        .client_handle
                        .as_ref()?
                        .read(cx)
                        .run_cell(&current_cell, cx)
                        .ok()?;
                }
                CellType::Chat => {
                    if let Err(err) = self.next_chat_turn(&current_cell, cx) {
                        error!("{:#?}", err);
                    }
                }
                _ => {}
            }
        });
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
                let current_cell = cells.get_cell_by_buffer(&buffer_handle, cx)?.clone();
                let current_cell_id = current_cell.id.get();

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

                let buffers_with_range_by_offset = vec![0..range.start, range.end..snapshot.len()]
                    .into_iter()
                    .map(|range| {
                        let text = snapshot.text_for_range(range.clone()).collect::<String>();
                        (text, range)
                    })
                    .map(|(text, range)| {
                        let buffer = cx.new_model(|cx| {
                            let mut buffer = Buffer::local(text, cx);
                            buffer.set_language(notebook.language.clone(), cx);
                            buffer
                        });
                        (buffer, range)
                    })
                    .zip(vec![0, 2])
                    .collect_vec();

                current_cell.source.update(cx, |buffer, cx| {
                    let edits = buffers_with_range_by_offset
                        .iter()
                        .map(|((_, range), _)| (range.clone(), ""));
                    buffer.edit(edits, None, cx);
                });

                for ((buffer, _), id_offset) in buffers_with_range_by_offset {
                    let cell = CellBuilder::new(u64::from(current_cell_id) + id_offset as u64)
                        .source(buffer)
                        .build(cx);
                    cells.insert(vec![cell], cx);
                }

                let _ = notebook
                    .client_handle
                    .as_ref()?
                    .read_with(cx, |client, cx| client.run_cell(&current_cell, cx));
            });
        });
    }

    fn insert_cell_above(
        &mut self,
        _cmd: &actions::InsertCellAbove,
        cx: &mut ViewContext<Self>,
    ) -> () {
        self.insert_cell(Bias::Left, None, cx);
    }

    fn insert_cell_below(
        &mut self,
        _cmd: &actions::InsertCellBelow,
        cx: &mut ViewContext<Self>,
    ) -> () {
        self.insert_cell(Bias::Right, None, cx);
    }

    fn insert_cell<'cx>(
        &mut self,
        bias: Bias,
        cell_type: Option<CellType>,
        cx: &'cx mut ViewContext<Self>,
    ) -> Option<CellId> {
        let Some((_, buffer_handle, _)) = self.editor.read(cx).active_excerpt(cx) else {
            return None;
        };

        let mut current_cell_id = self.notebook.read_with(cx, |notebook, cx| {
            let current_cell = notebook.cells.get_cell_by_buffer(&buffer_handle, cx)?;
            Some(current_cell.id.get().clone())
        })?;
        let new_cell_id = match bias {
            Bias::Left => current_cell_id,
            Bias::Right => current_cell_id.pre_inc(),
        };
        let source = cx.new_model(|cx| Buffer::local("\n", cx));
        let cell = CellBuilder::new(new_cell_id.into())
            .source(source)
            .cell_type(cell_type.unwrap_or_default())
            .build(cx);

        let _ = self.notebook.update(cx, |notebook, cx| {
            match cell.cell_type {
                CellType::Code => {
                    notebook.try_set_source_languages(cx, Some(vec![&cell]));
                }
                _ => {}
            }
            notebook.cells.insert(vec![cell], cx);
        });
        self.focus_cell(&new_cell_id, None, cx);
        Some(new_cell_id)
    }

    fn focus_cell<C: VisualContext>(&mut self, cell_id: &CellId, point: Option<Point>, cx: &mut C) {
        self.notebook.update(cx, |notebook, cx| {
            notebook.cells.sync(cx);
        });
        self.editor.update(cx, |editor, cx| {
            let Some(cell) = self.notebook.read(cx).cells.get_cell_by_id(cell_id).clone() else {
                return;
            };
            let notebook = self.notebook.read(cx);
            let mut cursor = notebook.cells.cursor::<CellId>();
            let mut point = cursor
                .summary::<CellId, TextSummary>(&cell.id.get(), Bias::Left, &())
                .lines;
            point.column = 0;
            drop(cursor);
            editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                s.select_ranges([point..point])
            });
        });
    }

    fn new_chat_cell(&mut self, _cmd: &actions::NewChatCell, cx: &mut ViewContext<Self>) -> () {
        self.insert_cell(Bias::Right, Some(CellType::Chat), cx);
    }

    fn next_chat_turn<'cx>(&mut self, cell: &Cell, cx: &mut ViewContext<'cx, Self>) -> Result<()> {
        let chat = &mut self.chat;
        let buffer_handle = cx.new_model(|cx| {
            let buffer = Buffer::local("", cx);
            let Some(registry) = chat.language_registry.as_ref() else {
                return buffer;
            };
            let markdown = registry.language_for_name("markdown");
            cx.spawn(|buffer_handle, mut cx| async move {
                let markdown = markdown.await?;
                buffer_handle.update(&mut cx, |buffer, cx| {
                    buffer.set_language(Some(markdown), cx)
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);

            buffer
        });

        self.notebook.update(cx, |notebook, cx| {
            let mut updated = cell.clone();
            updated.output_content.replace(buffer_handle.clone());
            updated
                .state
                .replace(cell::ExecutionState::Running("".into()));
            updated.update_titles(cx);
            notebook.cells.replace(updated, cx);
        });

        let msg = LanguageModelRequestMessage {
            role: assistant::Role::User,
            content: cell.source.read(cx).text(),
        };
        chat.messages.push(msg);
        let request = LanguageModelRequest {
            model: chat.model.clone(),
            messages: chat.history(),
            stop: vec![],
            temperature: 1.0,
        };

        let buffer_id = buffer_handle.read(cx).remote_id();
        let cell_id = cell.id.get();
        let stream = CompletionProvider::global(cx).complete(request);
        let stream_task = cx.spawn(|this, mut cx| async move {
            let mut messages = stream.await?;
            let mut offset = 0;
            while let Some(message) = messages.next().await {
                let mut content = message?;

                if let Err(err) = this.update_inner(&mut cx, |multi: &mut MultiBuffer, cx| {
                    let Some(buffer_handle) = multi.buffer(buffer_id) else {
                        return Err(anyhow!("Failed to obtain buffer handle"));
                    };

                    buffer_handle.update(cx, |buffer, cx| {
                        let lines = buffer.text_summary().lines;
                        // TODO: How do I set `SoftWrap` for a specific excerpt?
                        do_in!(|| {
                            let no_break_chars = vec![".", ",", "(", ")", "[", "]", "'", "`"];
                            let can_break = !no_break_chars.iter().contains(&content.get(0..1)?);
                            if lines.column + content.len() as u32 > 96 && can_break {
                                buffer.edit([(offset..offset, "\n")], None, cx);
                                if content.starts_with(" ") {
                                    content.remove(0);
                                }
                                offset += 1;
                            }
                        });
                        buffer.edit([(offset..offset, content.as_str())], None, cx);
                        offset += content.len();
                    });
                    anyhow::Ok(())
                }) {
                    error!("{:#?}", err)
                };
                smol::future::yield_now().await;
            }

            buffer_handle.update(&mut cx, |buffer, cx| {
                buffer.set_text(buffer.text() + "\n", cx);
            })?;

            this.update_inner(&mut cx, |notebook: &mut Notebook, cx| {
                do_in!(|| {
                    let mut updated = notebook.cells.get_cell_by_id(&cell_id)?.clone();
                    updated
                        .state
                        .replace(cell::ExecutionState::Succeeded("".into()));
                    updated.update_titles(cx);
                    notebook.cells.replace(updated, cx);
                });
            });

            anyhow::Ok(())
        });

        cx.spawn(|this, mut cx| async move {
            if let Err(err) = stream_task.await {
                this.update_inner(&mut cx, |notebook: &mut Notebook, cx| {
                    do_in!(|| {
                        let mut updated = notebook.cells.get_cell_by_id(&cell_id)?.clone();
                        updated
                            .state
                            .replace(cell::ExecutionState::Failed("".into()));
                        updated.update_titles(cx);
                        notebook.cells.replace(updated, cx);
                    });
                })?;
            };
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);

        Ok(())
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
            .on_action(cx.listener(NotebookEditor::insert_cell_above))
            .on_action(cx.listener(NotebookEditor::insert_cell_below))
            .on_action(cx.listener(NotebookEditor::new_chat_cell))
            .on_action(cx.listener(NotebookEditor::run_current_selection))
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
