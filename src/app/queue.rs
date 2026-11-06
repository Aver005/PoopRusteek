//! Очередь сообщений: то, что человек набрал, пока шёл ход. Сообщение несёт
//! свои файлы с собой, поэтому его можно и отправить в свой чат, и вернуть в
//! поле ввода целиком, не трогая то, что лежит в других чатах.

use super::conversation::{ConversationId, QueuedMessage};
use super::input::FileChip;
use super::{App, AppState};
use crate::error::AppResult;
use crate::provider::AttachedFile;

impl AppState {
    /// Вернуть очередь фокусного чата в поле ввода: текст, файловые чипы
    /// настоящими вложениями и файлы `/attach`. Сколько сообщений вернулось.
    pub(crate) fn restore_queued_to_input(&mut self) -> usize {
        let queued: Vec<QueuedMessage> = self.focused_mut().queued.drain(..).collect();
        let count = queued.len();
        for message in queued {
            self.input.append_restored(&message.text, message.chips);
            for file in message.named {
                crate::app::attachments::push_unique(&mut self.attached_files, file);
            }
        }
        self.focused_mut().queue_paused = false;
        count
    }
}

/// Чей очереди пора идти: беседа не занята, очередь не приостановлена и это
/// не побочный чат. Берётся первая по порядку — чтобы порядок был стабильным.
pub(crate) fn next_ready_queue(
    conversations: &super::conversation::Conversations,
) -> Option<ConversationId> {
    conversations
        .iter()
        .find(|conv| {
            !conv.queued.is_empty()
                && !conv.queue_paused
                && !conv.is_busy()
                && !conv.is_background_kind()
        })
        .map(|conv| conv.id)
}

/// Файлы сообщения из его чипов: то, что человек перетащил в поле.
pub(crate) fn chip_files(chips: &[FileChip]) -> Vec<AttachedFile> {
    chips
        .iter()
        .flat_map(|chip| chip.files.iter().cloned())
        .collect()
}

impl App {
    /// Отправить текст человека ходом в беседе `conversation`: развернуть
    /// `@файл`, собрать вложения и запустить ход. Файлы приходят с сообщением
    /// (`chips` — перетащенные, `named` — из `/attach`): общего списка
    /// вложений эта функция не читает и не трогает, а заметки пишет в свою
    /// беседу, а не в ту, что на экране.
    pub(crate) async fn send_user_text(
        &mut self,
        conversation: ConversationId,
        input: String,
        chips: Vec<FileChip>,
        named: Vec<AttachedFile>,
    ) -> AppResult<()> {
        let killed = self.state.background.cleanup_turn_of(conversation).await;
        if killed > 0
            && let Some(target) = self.state.conversations.get_mut(conversation)
        {
            target
                .messages
                .push(crate::provider::ChatMessage::system(&format!(
                    "Cleaned {killed} ephemeral job(s) before the new turn."
                )));
        }
        let expanded = self.expand_file_mentions(&input);
        let provider = self
            .state
            .conversations
            .get(conversation)
            .and_then(|conv| conv.provider.clone());
        let accepts = |path: &std::path::Path| {
            provider
                .as_ref()
                .is_some_and(|provider| provider.accepts_attachment(path))
        };
        let message = turn_message(
            &expanded,
            &input,
            &chips,
            named,
            &self.state.workspace(),
            &accepts,
        );
        // Человек написал сам — цепочка автоматических побудок оборвана.
        self.reset_timer_wakes(conversation);
        self.send_turn(conversation, Some(message)).await
    }

    /// Отправить следующее сообщение той очереди, чей чат освободился. Зовётся
    /// после каждой пачки событий — единственное место, где очередь уходит,
    /// поэтому новый путь, снимающий «занято», о ней помнить не обязан.
    /// Вернёт, было ли что отправлять.
    pub(crate) async fn drain_idle_queue(&mut self) -> bool {
        let Some(conversation) = next_ready_queue(&self.state.conversations) else {
            return false;
        };
        self.send_next_queued(conversation).await;
        true
    }

    /// Отправить следующее сообщение из очереди беседы, если оно есть.
    pub(crate) async fn send_next_queued(&mut self, conversation: ConversationId) {
        let Some(target) = self.state.conversations.get_mut(conversation) else {
            return;
        };
        if target.is_busy() || target.is_background_kind() {
            return;
        }
        let Some(next) = target.queued.pop_front() else {
            return;
        };
        target.queue_paused = false;
        if let Err(error) = self
            .send_user_text(conversation, next.text, next.chips, next.named)
            .await
            && let Some(target) = self.state.conversations.get_mut(conversation)
        {
            target
                .messages
                .push(crate::provider::ChatMessage::ui_system(&format!(
                    "Queued message failed: {error}"
                )));
        }
    }
}

/// Сообщение для модели: текст, чипы и `@файл` из текста, файлы `/attach`.
/// Чистая функция — вся «память» об окружении приходит аргументами.
fn turn_message(
    expanded: &str,
    typed: &str,
    chips: &[FileChip],
    mut named: Vec<AttachedFile>,
    workspace: &std::path::Path,
    accepts: &dyn Fn(&std::path::Path) -> bool,
) -> crate::provider::ChatMessage {
    let chip_files = chip_files(chips);
    // Прикреплённые через `/attach` и `@` в тексте не видны — их назовёт
    // сводка; чипы и `@файл` видны и так.
    named.retain(|file| !chip_files.iter().any(|chip| chip.path == file.path));
    let mut files = named.clone();
    let mentioned = crate::app::attachments::binary_mentions(typed, workspace);
    for file in chip_files.into_iter().chain(mentioned) {
        crate::app::attachments::push_unique(&mut files, file);
    }
    // Содержимое файлов уходит модели, а не в чат: рисовать 2 МБ лога
    // в ленте (и пересчитывать их каждый кадр) никому не нужно.
    crate::app::attachments::user_message(expanded, typed, &files, &named, accepts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::conversation::Conversation;

    fn file(name: &str) -> AttachedFile {
        AttachedFile {
            path: format!("/work/{name}"),
            display_name: name.to_string(),
            size: 10,
            is_image: false,
        }
    }

    fn queued(text: &str) -> QueuedMessage {
        QueuedMessage {
            text: text.to_string(),
            chips: Vec::new(),
            named: Vec::new(),
        }
    }

    fn state() -> AppState {
        AppState::new(Conversation::fresh_main(None))
    }

    /// Esc с файлами: текст возвращается вместе с чипом, а чип снова даёт файл.
    #[test]
    fn esc_returns_the_text_and_turns_the_chip_back_into_a_real_attachment() {
        let mut state = state();
        state.input.buffer = "see ".to_string();
        state.input.cursor = 4;
        state.input.insert_files(vec![file("a.pdf")]);
        let text = state.input.expanded();
        let chips = state.input.present_chips();
        state.input.clear_buffer();
        state.focused_mut().queued.push_back(QueuedMessage {
            text,
            chips,
            named: Vec::new(),
        });

        assert_eq!(state.restore_queued_to_input(), 1);
        assert!(state.input.buffer.contains("[📎 a.pdf]"));
        let files = state.input.chip_files();
        assert_eq!(files.len(), 1, "чип снова держит файл, а не литерал");
        assert_eq!(files[0].display_name, "a.pdf");
        assert!(state.focused().queued.is_empty());
    }

    #[test]
    fn two_queued_chips_with_the_same_name_do_not_merge_on_restore() {
        let mut state = state();
        for _ in 0..2 {
            let mut input = crate::app::input::InputState::default();
            input.insert_files(vec![file("a.pdf")]);
            state.focused_mut().queued.push_back(QueuedMessage {
                text: input.expanded(),
                chips: input.present_chips(),
                named: Vec::new(),
            });
        }
        state.restore_queued_to_input();
        assert_eq!(state.input.chip_files().len(), 2);
        assert_eq!(
            state.input.buffer.matches("[📎 a.pdf").count(),
            2,
            "{}",
            state.input.buffer
        );
    }

    #[test]
    fn attach_files_come_back_to_the_attachment_bar_without_duplicates() {
        let mut state = state();
        state.attached_files.push(file("notes.md"));
        state.focused_mut().queued.push_back(QueuedMessage {
            named: vec![file("notes.md"), file("todo.md")],
            ..queued("go")
        });
        state.restore_queued_to_input();
        assert_eq!(state.attached_files.len(), 2);
    }

    #[test]
    fn restoring_returns_every_message_in_order() {
        let mut state = state();
        state.focused_mut().queued.push_back(queued("first"));
        state.focused_mut().queued.push_back(queued("second"));
        assert_eq!(state.restore_queued_to_input(), 2);
        assert_eq!(state.input.buffer, "first\nsecond");
    }

    /// Файлы сообщения из очереди — только его собственные.
    #[test]
    fn a_queued_message_carries_its_own_files_into_the_turn() {
        let mut input = crate::app::input::InputState::default();
        input.insert_files(vec![file("a.pdf")]);
        let chips = input.present_chips();
        let message = turn_message(
            "see",
            "see",
            &chips,
            vec![file("own.md")],
            std::path::Path::new("."),
            &|_| false,
        );
        assert!(message.content.contains("a.pdf"), "{}", message.content);
        assert!(message.content.contains("own.md"), "{}", message.content);
        assert!(!message.content.contains("notes.md"));
    }

    #[test]
    fn a_queue_in_a_background_chat_is_never_picked() {
        let mut state = state();
        let mut agent = Conversation::fresh_main(None);
        agent.kind = crate::app::conversation::ConversationKind::SubAgent;
        agent.queued.push_back(queued("x"));
        state.conversations.add_background(agent);
        assert_eq!(next_ready_queue(&state.conversations), None);
    }

    #[test]
    fn a_ready_queue_is_picked_only_when_the_chat_is_idle() {
        let mut state = state();
        state.focused_mut().queued.push_back(queued("next"));
        let id = state.conversations.focused_id();
        assert_eq!(next_ready_queue(&state.conversations), Some(id));
        state.focused_mut().generation.active = true;
        assert_eq!(next_ready_queue(&state.conversations), None);
        state.focused_mut().generation.active = false;
        state.focused_mut().compacting = Some(3);
        assert_eq!(next_ready_queue(&state.conversations), None);
    }
}
