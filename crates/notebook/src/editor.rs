//! Jupyter support for Zed.

use editor::{
    items::entry_label_color, Editor, EditorEvent, ExcerptId, ExcerptRange, MultiBuffer,
    MAX_TAB_TITLE_LEN,
};
use gpui::{
    AnyView, AppContext, Context, EventEmitter, FocusHandle, FocusableView, HighlightStyle, Model,
    ParentElement, Subscription, View,
};
use itertools::Itertools;
use language::{self, Buffer, Capability, HighlightId};
use project::{self, Project};
use std::{
    any::{Any, TypeId},
    convert::AsRef,
    ops::Range,
};
use theme::{ActiveTheme, SyntaxTheme};
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, ViewContext, VisualContext,
};

use util::paths::PathExt;
use workspace::item::{ItemEvent, ItemHandle};

use crate::{
    actions,
    cell::{Cell, CellId, ForOutput, IpynbCodeOutput, Output, StreamOutputTarget},
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
        let cells = notebook
            .read(cx)
            .cells
            .iter()
            .map(|cell| cell.clone())
            .collect_vec();

        let multi = cx.new_model(|model_cx| {
            let mut multi = MultiBuffer::new(0, Capability::ReadWrite);
            for (cell, range) in cells
                .into_iter()
                .map(|cell| {
                    let range = ExcerptRange {
                        context: Range {
                            start: 0 as usize,
                            end: cell.source.read(model_cx).len() as _,
                        },
                        primary: None,
                    };
                    (cell, range)
                })
                .collect_vec()
            {
                let id: u64 = cell.id.into();
                let prev_excerpt_id = if id == 1 {
                    ExcerptId::min()
                } else {
                    ExcerptId::from_proto(id - 1)
                };
                multi.insert_excerpts_with_ids_after(
                    prev_excerpt_id,
                    cell.source,
                    vec![(cell.id.into(), range)],
                    model_cx,
                );
            }

            multi
        });

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

        let subscription = cx.subscribe(&project, |this, project, event, cx| {
            log::info!("Event: {:#?}", event);
        });

        cx.subscribe(&editor, |this, _editor, event: &EditorEvent, cx| {
            cx.emit(event.clone());
            log::info!("Event: {:#?}", event);
        })
        .detach();

        let this = NotebookEditor {
            notebook,
            editor,
            focus_handle,
            _subscriptions: [subscription].into(),
        };

        this
    }

    fn highlight_syntax(
        &mut self,
        only_for_excerpt_ids: Option<Vec<ExcerptId>>,
        cx: &mut ViewContext<NotebookEditor>,
    ) {
        let mut highlights_by_range = Vec::<(Range<usize>, HighlightId)>::default();

        self.editor.update(cx, |editor, cx| {
            editor.buffer().read_with(cx, |multi, cx| {
                // This is somewhat inefficient but OK for now.
                multi.for_each_buffer(|buffer_handle| {
                    if only_for_excerpt_ids.is_some()
                        && (!only_for_excerpt_ids.as_ref().unwrap().iter().any(|id| {
                            multi
                                .excerpts_for_buffer(buffer_handle, cx)
                                .iter()
                                .map(|(id, _)| id)
                                .contains(id)
                        }))
                    {
                        return;
                    }
                    buffer_handle.read_with(cx, |buffer, cx| {
                        highlights_by_range.extend(self.get_highlight_ids_for_buffer(buffer, cx)?);
                        Some(())
                    });
                });
            });
        });

        self.editor.update(cx, |editor, cx| {
            let styles_by_range = NotebookEditor::get_highlight_styles_for_multi(
                editor,
                highlights_by_range,
                &only_for_excerpt_ids,
                cx,
            );

            for (rng, style) in styles_by_range {
                editor.highlight_text::<NotebookEditor>(vec![rng], style, cx);
            }
        });
    }

    fn get_highlight_ids_for_buffer(
        &self,
        buffer: &Buffer,
        cx: &AppContext,
    ) -> Option<Vec<(Range<usize>, HighlightId)>> {
        let lang = self.notebook.read(cx).language.clone()?;
        let highlights_by_range = lang
            .highlight_text(buffer.as_rope(), 0..buffer.len())
            .into_iter()
            .collect_vec();

        Some(highlights_by_range)
    }

    fn get_highlight_styles_for_multi<T>(
        editor: &Editor,
        highlights_by_range: Vec<(Range<usize>, HighlightId)>,
        only_for_excerpt_ids: &Option<Vec<ExcerptId>>,
        cx: &ViewContext<T>,
    ) -> Vec<(Range<multi_buffer::Anchor>, HighlightStyle)> {
        let multi = editor.buffer().read(cx);
        let syntax = cx.theme().syntax().as_ref().clone();

        highlights_by_range
            .iter()
            .flat_map(|(range, h)| {
                multi
                    .range_to_buffer_ranges(range.clone(), cx)
                    .iter()
                    .filter_map(|(_buffer, range, excerpt_id)| {
                        if only_for_excerpt_ids.is_some()
                            && (!only_for_excerpt_ids
                                .as_ref()
                                .unwrap()
                                .iter()
                                .any(|id| id == excerpt_id))
                        {
                            return None;
                        }
                        let range = Range {
                            start: multi.snapshot(cx).anchor_before(range.start),
                            end: multi.snapshot(cx).anchor_before(range.end),
                        };
                        let style = h.style(&syntax)?;
                        Some((range, style))
                    })
                    .collect_vec()
            })
            .collect_vec()
    }

    fn expand_cell_outputs(&mut self, cx: &mut ViewContext<Editor>) {
        let editor = self.editor.read(cx);

        let outputs: Vec<Output> = self.notebook.read(cx).cells.iter().flat_map(|cell| {
            let Some(outputs) = &cell.outputs else {
                return None;
            };
            outputs.iter().filter_map(|output| {
                use IpynbCodeOutput::*;
                match output {
                    Stream {
                        output_type,
                        name,
                        text,
                    } => match name {
                        StreamOutputTarget::Stdout => ForOutput::print(Some(text)),
                        StreamOutputTarget::Stderr => ForOutput::print(None),
                    },
                    DisplayData {
                        output_type,
                        data,
                        metadata,
                    } => ForOutput::print(None),
                    ExecutionResult {
                        // TODO: Handle MIME types here
                        output_type,
                        execution_count,
                        data,
                        metadata,
                    } => ForOutput::print(None),
                };
            })
        });
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
        let mut nb_editor = NotebookEditor::new(project, notebook, cx);
        nb_editor.highlight_syntax(None, cx);
        nb_editor
    }
}

pub fn init(cx: &mut AppContext) {
    workspace::register_project_item::<NotebookEditor>(cx);
}
