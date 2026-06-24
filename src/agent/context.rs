use crate::provider::ChatMessage;

pub struct ContextManager {
    messages: Vec<ChatMessage>,
    max_messages: usize,
    system_prompt: String,
}

impl ContextManager {
    pub fn new(max_messages: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_messages,
            system_prompt: String::new(),
        }
    }

    pub fn set_system_prompt(&mut self, prompt: String) {
        self.system_prompt = prompt;
    }

    pub fn add_message(&mut self, message: ChatMessage) {
        self.messages.push(message);
        self.trim();
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub fn build_messages(&self) -> Vec<ChatMessage> {
        let mut result = Vec::new();
        if !self.system_prompt.is_empty() {
            result.push(ChatMessage::system(&self.system_prompt));
        }
        result.extend(self.messages.clone());
        result
    }

    fn trim(&mut self) {
        if self.messages.len() > self.max_messages {
            let drain_count = self.messages.len() - self.max_messages;
            self.messages.drain(..drain_count);
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}
