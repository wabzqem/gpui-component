use anyhow::Result;
use gpui::{Context, EntityInputHandler, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, InlineCompletionContext,
    InlineCompletionItem, InlineCompletionResponse, InlineCompletionTriggerKind,
    request::Completion,
};
use ropey::Rope;
use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use crate::input::{
    InputState,
    popovers::{CompletionMenu, ContextMenu},
};

/// Default debounce duration for inline completions.
const DEFAULT_INLINE_COMPLETION_DEBOUNCE: Duration = Duration::from_millis(300);

/// A trait for providing code completions based on the current input state and context.
pub trait CompletionProvider {
    /// Fetches completions based on the given byte offset.
    ///
    /// - The `offset` is in bytes of current cursor.
    ///
    /// textDocument/completion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_completion
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>>;

    /// Fetches an inline completion suggestion for the given position.
    ///
    /// This is called after a debounce period when the user stops typing.
    /// The provider can analyze the text and cursor position to determine
    /// what inline completion suggestion to show.
    ///
    ///
    /// # Arguments
    /// * `rope` - The current text content
    /// * `offset` - The cursor position in bytes
    ///
    /// textDocument/inlineCompletion
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlineCompletion
    fn inline_completion(
        &self,
        _rope: &Rope,
        _offset: usize,
        _trigger: InlineCompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<Result<InlineCompletionResponse>> {
        Task::ready(Ok(InlineCompletionResponse::Array(vec![])))
    }

    /// Returns the debounce duration for inline completions.
    ///
    /// Default: 300ms
    #[inline]
    fn inline_completion_debounce(&self) -> Duration {
        DEFAULT_INLINE_COMPLETION_DEBOUNCE
    }

    fn resolve_completions(
        &self,
        _completion_indices: Vec<usize>,
        _completions: Rc<RefCell<Box<[Completion]>>>,
        _: &mut Context<InputState>,
    ) -> Task<Result<bool>> {
        Task::ready(Ok(false))
    }

    /// Determines if the completion should be triggered based on the given byte offset.
    ///
    /// This is called on the main thread.
    fn is_completion_trigger(
        &self,
        offset: usize,
        new_text: &str,
        cx: &mut Context<InputState>,
    ) -> bool;
}

pub(crate) struct InlineCompletion {
    /// Completion item to display as an inline completion suggestion
    pub(crate) item: Option<InlineCompletionItem>,
    /// Task for debouncing inline completion requests
    pub(crate) task: Task<Result<InlineCompletionResponse>>,
}

impl Default for InlineCompletion {
    fn default() -> Self {
        Self {
            item: None,
            task: Task::ready(Ok(InlineCompletionResponse::Array(vec![]))),
        }
    }
}

impl InputState {
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

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        // Always schedule inline completion (debounced).
        // It will check if menu is open before showing the suggestion.
        self.schedule_inline_completion(window, cx);

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

    /// Requests completion at the current cursor without changing the input text.
    ///
    /// Returns `true` when a completion provider is configured and the request was
    /// scheduled. Disabled inputs and inputs without a provider return `false`.
    pub fn request_completions(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.disabled || self.lsp.completion_provider.is_none() {
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

    pub(crate) fn on_action_show_completions(
        &mut self,
        _: &crate::input::ShowCompletions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.request_completions(window, cx) {
            cx.propagate();
        }
    }

    fn request_completions_at(
        &mut self,
        start: usize,
        new_offset: usize,
        trigger_kind: lsp_types::CompletionTriggerKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        let menu = match self.context_menu_content.as_ref() {
            Some(ContextMenu::Completion(menu)) => Some(menu),
            _ => None,
        };

        // Create or get the existing completion menu.
        let menu = match menu {
            Some(menu) => menu.clone(),
            None => {
                let menu = CompletionMenu::new(cx.entity(), window, cx);
                self.context_menu_content = Some(ContextMenu::Completion(menu.clone()));
                menu
            }
        };

        let invoked = trigger_kind == lsp_types::CompletionTriggerKind::INVOKED;
        let start_offset = if invoked {
            _ = menu.update(cx, |menu, cx| menu.hide(cx));
            new_offset
        } else {
            menu.read(cx).trigger_start_offset.unwrap_or(start)
        };
        if new_offset < start_offset {
            return;
        }

        let query = self
            .text_for_range(
                self.range_to_utf16(&(start_offset..new_offset)),
                &mut None,
                window,
                cx,
            )
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        _ = menu.update(cx, |menu, _| {
            menu.update_query(start_offset, query.clone());
        });

        let completion_context = CompletionContext {
            trigger_kind,
            trigger_character: (!invoked).then_some(query),
        };

        let provider_responses =
            provider.completions(&self.text, new_offset, completion_context, window, cx);
        self._context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let mut completions: Vec<CompletionItem> = vec![];
            if let Some(provider_responses) = provider_responses.await.ok() {
                match provider_responses {
                    CompletionResponse::Array(items) => completions.extend(items),
                    CompletionResponse::List(list) => completions.extend(list.items),
                }
            }

            if completions.is_empty() {
                _ = menu.update(cx, |menu, cx| {
                    menu.hide(cx);
                    cx.notify();
                });

                return Ok(());
            }

            editor
                .update_in(cx, |editor, window, cx| {
                    if !editor.focus_handle.is_focused(window) {
                        return;
                    }

                    _ = menu.update(cx, |menu, cx| {
                        menu.show(new_offset, completions, window, cx);
                    });

                    cx.notify();
                })
                .ok();

            Ok(())
        });
    }

    /// Schedule an inline completion request after debouncing.
    pub(crate) fn schedule_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clear any existing inline completion on text change
        self.clear_inline_completion(cx);

        let Some(provider) = self.lsp.completion_provider.clone() else {
            return;
        };

        let offset = self.cursor();
        let text = self.text.clone();
        let debounce = provider.inline_completion_debounce();
        let background_executor = cx.background_executor().clone();

        self.inline_completion.task = cx.spawn_in(window, async move |editor, cx| {
            // Debounce: wait before fetching to avoid unnecessary requests while typing
            background_executor.timer(debounce).await;

            // Now fetch the inline completion after the debounce period
            let task = editor.update_in(cx, |editor, window, cx| {
                // Check if cursor has moved during debounce
                if editor.cursor() != offset {
                    return None;
                }

                // Don't fetch if completion menu is open
                if editor.is_context_menu_open(cx) {
                    return None;
                }

                let trigger = InlineCompletionContext {
                    trigger_kind: InlineCompletionTriggerKind::Automatic,
                    selected_completion_info: None,
                };

                Some(provider.inline_completion(&text, offset, trigger, window, cx))
            })?;

            let Some(task) = task else {
                return Ok(InlineCompletionResponse::Array(vec![]));
            };

            let response = task.await?;

            editor.update_in(cx, |editor, _window, cx| {
                // Only apply if cursor still hasn't moved
                if editor.cursor() != offset {
                    return;
                }

                // Don't show if completion menu opened while we were fetching
                if editor.is_context_menu_open(cx) {
                    return;
                }

                if let Some(item) = match response.clone() {
                    InlineCompletionResponse::Array(items) => items.into_iter().next(),
                    InlineCompletionResponse::List(comp_list) => comp_list.items.into_iter().next(),
                } {
                    editor.inline_completion.item = Some(item);
                    cx.notify();
                }
            })?;

            Ok(response)
        });
    }

    /// Check if an inline completion suggestion is currently displayed.
    #[inline]
    pub(crate) fn has_inline_completion(&self) -> bool {
        self.inline_completion.item.is_some()
    }

    /// Clear the inline completion suggestion.
    pub(crate) fn clear_inline_completion(&mut self, cx: &mut Context<Self>) {
        self.inline_completion = InlineCompletion::default();
        cx.notify();
    }

    /// Accept the inline completion, inserting it at the cursor position.
    /// Returns true if a completion was accepted, false if there was none.
    pub(crate) fn accept_inline_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(completion_item) = self.inline_completion.item.take() else {
            return false;
        };

        let cursor = self.cursor();
        let range_utf16 = self.range_to_utf16(&(cursor..cursor));
        let completion_text = completion_item.insert_text;
        self.replace_text_in_range_silent(Some(range_utf16), &completion_text, window, cx);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{AppContext, Context, TestAppContext, VisualTestContext, Window};

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct ObservedCompletionRequest {
        text: String,
        offset: usize,
        context: CompletionContext,
    }

    struct RecordingCompletionProvider {
        observed: Rc<RefCell<Option<ObservedCompletionRequest>>>,
    }

    impl CompletionProvider for RecordingCompletionProvider {
        fn completions(
            &self,
            text: &Rope,
            offset: usize,
            context: CompletionContext,
            _: &mut Window,
            _: &mut Context<InputState>,
        ) -> Task<Result<CompletionResponse>> {
            self.observed.replace(Some(ObservedCompletionRequest {
                text: text.to_string(),
                offset,
                context,
            }));
            Task::ready(Ok(CompletionResponse::Array(Vec::new())))
        }

        fn is_completion_trigger(&self, _: usize, _: &str, _: &mut Context<InputState>) -> bool {
            false
        }
    }

    #[gpui::test]
    fn explicit_completion_requests_at_cursor_without_editing(cx: &mut TestAppContext) {
        let observed = Rc::new(RefCell::new(None));
        let provider = RecordingCompletionProvider {
            observed: observed.clone(),
        };
        let mut input = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                crate::init(cx);
                input = Some(cx.new(|cx| {
                    let mut state = InputState::new(window, cx).default_value("value.cl");
                    state.lsp.completion_provider = Some(Rc::new(provider));
                    state
                }));
                cx.new(|cx| crate::Root::new(input.clone().unwrap(), window, cx))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                state.set_selected_range("value.cl".len().."value.cl".len(), cx);
                assert!(state.request_completions(window, cx));
                assert_eq!(state.value().as_ref(), "value.cl");
            });
        });

        assert_eq!(
            observed.borrow().as_ref(),
            Some(&ObservedCompletionRequest {
                text: "value.cl".to_string(),
                offset: "value.cl".len(),
                context: CompletionContext {
                    trigger_kind: lsp_types::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                },
            })
        );
    }

    #[gpui::test]
    fn explicit_completion_is_disabled_without_a_mutable_provider(cx: &mut TestAppContext) {
        let mut input = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                crate::init(cx);
                input = Some(cx.new(|cx| InputState::new(window, cx)));
                cx.new(|cx| crate::Root::new(input.clone().unwrap(), window, cx))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);

        cx.update(|window, cx| {
            input.update(cx, |state, cx| {
                assert!(!state.request_completions(window, cx));
                state.lsp.completion_provider = Some(Rc::new(RecordingCompletionProvider {
                    observed: Rc::new(RefCell::new(None)),
                }));
                state.disabled = true;
                assert!(!state.request_completions(window, cx));
            });
        });
    }
}
