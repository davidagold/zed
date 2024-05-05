//! Jupyter support for Zed.

use editor::{
    items::entry_label_color, Editor, EditorEvent, ExcerptId, ExcerptRange, MultiBuffer,
    MAX_TAB_TITLE_LEN,
};
use gpui::{
    AnyView, AppContext, Context, EventEmitter, FocusHandle, FocusableView, HighlightStyle, Model,
    ModelContext, ParentElement, Subscription, View,
};
use itertools::Itertools;
use language::{self, Buffer, Capability, HighlightId};
use project::{self, Project};
use std::{
    any::{Any, TypeId},
    convert::AsRef,
    ops::Range,
};
use theme::ActiveTheme;
use ui::{
    div, h_flex, FluentBuilder, InteractiveElement, IntoElement, Label, LabelCommon, Render,
    SharedString, Styled, ViewContext, VisualContext,
};

use util::paths::PathExt;
use workspace::item::{ItemEvent, ItemHandle};

use crate::{
    actions,
    cell::{Cell, CellId, ForOutput, IpynbCodeOutput, StreamOutputTarget},
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
                .iter()
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
                    cell.source.clone(),
                    vec![(cell.id.into(), range)],
                    model_cx,
                );

                // TODO: Do something with `maybe_output`
                let output_buffers =
                    NotebookEditor::get_cell_output_buffers(&cells, &mut multi, None, model_cx);
                for output in output_buffers {
                    log::info!("{:#?}", output);
                }
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
            match event {
                EditorEvent::ScrollPositionChanged { local, autoscroll } => {}
                _ => {
                    log::info!("Event: {:#?}", event);
                }
            }
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

    fn get_cell_output_buffers(
        cells: &Vec<Cell>,
        multi: &mut MultiBuffer,
        for_cell_ids: Option<Vec<CellId>>,
        cx: &mut ModelContext<MultiBuffer>,
    ) -> Vec<Option<(ExcerptRange<usize>, Model<Buffer>)>> {
        let output_actions: Vec<Option<ForOutput>> = cells
            .iter()
            .flat_map(|cell| {
                let Some(outputs) = &cell.outputs else {
                    return Vec::new();
                };
                outputs
                    .into_iter()
                    .map(|output| {
                        use IpynbCodeOutput::*;
                        let output_action = match output {
                            Stream { name, text } => {
                                log::info!("Output text: {:#?}", text);
                                match name {
                                    StreamOutputTarget::Stdout => {
                                        ForOutput::print(Some(text.clone()))
                                    }
                                    StreamOutputTarget::Stderr => ForOutput::print(None),
                                }
                            }
                            DisplayData { data, metadata } => ForOutput::print(None),
                            ExecutionResult {
                                // TODO: Handle MIME types here
                                execution_count,
                                data,
                                metadata,
                            } => ForOutput::print(None),
                        };
                        Some(output_action)
                    })
                    .collect()
            })
            .collect();

        let buffers_by_range: Vec<Option<(ExcerptRange<usize>, Model<Buffer>)>> = output_actions
            .into_iter()
            .map(|maybe_output_action| match maybe_output_action {
                Some(ForOutput::Print(Some(text))) => {
                    let range = ExcerptRange {
                        context: 0..text.len(),
                        primary: None,
                    };
                    let buffer = cx.new_model(|cx| Buffer::local(text, cx));
                    Some((range, buffer))
                }
                _ => None,
            })
            .collect();

        buffers_by_range
        // for (excerpt_id, range, buffer) in buffers_by_range {
        //     multi.insert_excerpts_after(excerpt_id, buffer, vec![range], cx);
        // }
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
        NotebookEditor::new(project, notebook, cx)
    }
}

pub fn init(cx: &mut AppContext) {
    workspace::register_project_item::<NotebookEditor>(cx);
}
