use std::{ops::Range, rc::Rc};

use anyhow::Result;
use gpui::{Context, Task, Window};
use lsp_types::{CompletionContext, CompletionResponse};

use super::InputState;
use crate::input::{CompletionMenuState, CompletionProvider, InputExtras};

/// Optional menu completion for a single-line field.
///
/// This deliberately carries only a completion provider and its menu state. It
/// is not an LSP client and does not add source-editor facilities to InputState.
pub struct InputCompletionExtras {
    pub(crate) provider: Option<Rc<dyn CompletionProvider>>,
    pub(crate) menu: CompletionMenuState,
    pub(crate) task: Task<Result<()>>,
}

impl InputExtras for InputCompletionExtras {}

impl Default for InputCompletionExtras {
    fn default() -> Self {
        Self {
            provider: None,
            menu: CompletionMenuState::default(),
            task: Task::ready(Ok(())),
        }
    }
}

impl InputState {
    /// Configures lightweight menu completion for this single-line field.
    ///
    /// LSP and source-editor features remain exclusive to EditorState.
    pub fn set_completion_provider(&mut self, provider: Rc<dyn CompletionProvider>) {
        self.extras.provider = Some(provider);
    }

    /// Requests completion at the current cursor. Returns false when no
    /// provider is configured or the input is disabled.
    pub fn request_completions(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.disabled || self.extras.provider.is_none() {
            return false;
        }
        let offset = self.cursor();
        self.request_completions_at(
            offset,
            offset,
            lsp_types::CompletionTriggerKind::INVOKED,
            window,
            cx,
        );
        true
    }

    pub(crate) fn on_action_show_input_completions(
        &mut self,
        _: &crate::input::ShowCompletions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.request_completions(window, cx) {
            cx.propagate();
        }
    }

    pub(crate) fn handle_completion_trigger(
        &mut self,
        range: &Range<usize>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.completion_inserting {
            return;
        }
        let Some(provider) = self.extras.provider.clone() else {
            return;
        };
        let start = range.end;
        if !provider.is_completion_trigger(start, new_text, cx) {
            return;
        }
        self.request_completions_at(
            start,
            self.cursor(),
            lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER,
            window,
            cx,
        );
    }

    fn request_completions_at(
        &mut self,
        start: usize,
        new_offset: usize,
        trigger_kind: lsp_types::CompletionTriggerKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.extras.provider.clone() else {
            return;
        };

        let invoked = trigger_kind == lsp_types::CompletionTriggerKind::INVOKED;
        let start_offset = if invoked {
            let menu = &mut self.extras.menu;
            menu.open = false;
            menu.items.clear();
            menu.trigger_start_offset = Some(new_offset);
            menu.query.clear();
            menu.bump();
            new_offset
        } else {
            let start_offset = self.extras.menu.trigger_start_offset.unwrap_or(start);
            if new_offset < start_offset {
                self.hide_completion_menu(cx);
                return;
            }
            start_offset
        };
        let query = self.text.slice(start_offset..new_offset).to_string();
        self.extras.menu.trigger_start_offset = Some(start_offset);
        self.extras.menu.query.clone_from(&query);
        let completion_context = CompletionContext {
            trigger_kind,
            trigger_character: (!invoked).then_some(query),
        };
        let responses =
            provider.completions(&self.text, new_offset, completion_context, window, cx);
        self.extras.task = cx.spawn_in(window, async move |input, cx| {
            let mut items = Vec::new();
            if let Some(response) = responses.await.ok() {
                match response {
                    CompletionResponse::Array(response_items) => items.extend(response_items),
                    CompletionResponse::List(response_items) => items.extend(response_items.items),
                }
            }
            input
                .update_in(cx, |input, window, cx| {
                    if !input.focus_handle.is_focused(window) {
                        return;
                    }
                    input.extras.menu.items = items;
                    input.extras.menu.open = !input.extras.menu.items.is_empty();
                    input.extras.menu.bump();
                    cx.notify();
                })
                .ok();
            Ok(())
        });
    }

    #[doc(hidden)]
    pub fn completion_menu_state(&self) -> &CompletionMenuState {
        &self.extras.menu
    }

    pub(crate) fn hide_completion_menu(&mut self, cx: &mut Context<Self>) {
        self.extras.menu.open = false;
        self.extras.task = Task::ready(Ok(()));
        cx.notify();
    }

    pub fn dismiss_completion_overlay(&mut self, cx: &mut Context<Self>) {
        if self.extras.menu.open {
            self.extras.menu.open = false;
            cx.notify();
        }
    }

    pub(crate) fn is_completion_menu_open(&self) -> bool {
        self.extras.menu.open
    }

    pub(crate) fn handle_completion_menu_action(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.extras.menu.open {
            return false;
        }
        let Some(handler) = self.overlay_action_handler.clone() else {
            return false;
        };
        let closes_overlay =
            crate::input::Enter::is_primary(&*action) || action.partial_eq(&crate::input::Escape);
        let handled = handler(
            crate::input::InputOverlayKind::Completion,
            action,
            window,
            cx,
        );
        if handled && closes_overlay {
            self.extras.menu.open = false;
            cx.notify();
        }
        handled
    }
}
