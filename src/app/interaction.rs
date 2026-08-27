//! Очередь запросов к человеку: одно на экране, остальные ждут в FIFO.
//! Перезапись занятого слота осиротила бы предыдущий запрос на `Notify`.

use super::events::{
    Modal, PendingInteraction, QuestionRequest, QuestionState, ToolApprovalRequest,
};
use super::{App, conversation};

impl App {
    /// Запрос на подтверждение инструмента: разрешить сразу, поставить в
    /// очередь или показать.
    pub(super) async fn on_tool_approval_requested(&mut self, request: ToolApprovalRequest) {
        if self.state.approved_tools.contains(&request.tool_name) {
            request.resolve(true).await;
            self.state.focused_mut().generation.active = true;
            self.state.status_message = format!("Running {} (auto-approved)", request.tool_name);
            return;
        }
        if self.slot_is_busy() {
            self.state
                .pending_interactions
                .push_back(PendingInteraction::Approval(request));
            self.state.status_message = format!(
                "{} interaction(s) queued",
                self.state.pending_interactions.len()
            );
            return;
        }
        self.present_tool_approval(request);
    }

    /// Вопрос от модели — та же развилка, но без списка разрешённых.
    pub(super) fn on_question_requested(&mut self, request: QuestionRequest, state: QuestionState) {
        if self.slot_is_busy() {
            // Строку состояния не трогаем: на экране висит модалка, и её
            // текст нужнее счётчика очереди.
            self.state
                .pending_interactions
                .push_back(PendingInteraction::Question(request, state));
            return;
        }
        self.present_question(request, state);
    }

    /// Занят ли экран чем-то, что ждёт ответа человека.
    fn slot_is_busy(&self) -> bool {
        self.state.modal.is_some()
            || self.state.pending_tool_approval.is_some()
            || self.state.pending_question.is_some()
    }

    /// Показать запрос подтверждения: модалка плюс текущий слот.
    fn present_tool_approval(&mut self, request: ToolApprovalRequest) {
        self.state.focused_mut().generation.active = false;
        self.state.status_message = format!("Approve tool {}?", request.tool_name);
        self.state.modal = Some(Modal::ToolApproval {
            tool_name: request.tool_name.clone(),
            arguments: request.arguments.clone(),
            scroll_offset: 0,
            always_allow: false,
        });
        self.state.pending_tool_approval = Some(request);
    }

    /// То же для вопроса от модели.
    fn present_question(&mut self, request: QuestionRequest, state: QuestionState) {
        self.state.pending_question = Some(request);
        self.state.modal = Some(Modal::Question(state));
        self.state.focused_mut().generation.active = false;
        self.state.status_message = "Question pending...".to_string();
    }

    /// Достать следующее из очереди. Разрешённое тем временем подтверждение
    /// закрывается само, не показываясь.
    pub(crate) async fn present_next_interaction(&mut self) {
        while let Some(next) = self.state.pending_interactions.pop_front() {
            match next {
                PendingInteraction::Approval(request) => {
                    if self.state.approved_tools.contains(&request.tool_name) {
                        request.resolve(true).await;
                        continue;
                    }
                    self.present_tool_approval(request);
                    return;
                }
                PendingInteraction::Question(request, state) => {
                    self.present_question(request, state);
                    return;
                }
            }
        }
    }

    /// Deny-and-drop every pending approval belonging to `conversation` — its
    /// turn is being cancelled, so leaving them queued (or on screen) would
    /// present approvals for a task that no longer exists.
    pub(crate) async fn purge_interactions_for(
        &mut self,
        conversation: conversation::ConversationId,
    ) {
        let mut kept = std::collections::VecDeque::new();
        while let Some(item) = self.state.pending_interactions.pop_front() {
            match item {
                PendingInteraction::Approval(request) if request.conversation == conversation => {
                    request.resolve(false).await;
                }
                other => kept.push_back(other),
            }
        }
        self.state.pending_interactions = kept;

        let current_is_target = self
            .state
            .pending_tool_approval
            .as_ref()
            .is_some_and(|r| r.conversation == conversation);
        if current_is_target {
            if let Some(request) = self.state.pending_tool_approval.take() {
                request.resolve(false).await;
            }
            if matches!(self.state.modal, Some(Modal::ToolApproval { .. })) {
                self.state.modal = None;
            }
            self.present_next_interaction().await;
        }
    }
}
