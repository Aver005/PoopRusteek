//! Режим веб-чата DeepSeek из id модели. Модель (Instant/Expert), DeepThink и
//! веб-поиск — три независимых переключателя сайта; id складывает их суффиксами.
//!
//! Грамматика: `deepseek-chat` | `deepseek-expert` | `deepseek-reasoner`, затем
//! по желанию `-think` и `-search`. `deepseek-reasoner` — прежнее имя Expert с
//! DeepThink, оставлено ради старых конфигов.

/// Модель, которой сайт отвечает (`model_type` на проводе).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// Быстрая модель, на проводе `default`.
    Instant,
    /// Сильная и медленная, на проводе `expert`.
    Expert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepseekMode {
    pub model: ModelKind,
    pub thinking: bool,
    pub search: bool,
}

const PREFIX: &str = "deepseek-";
const THINK: &str = "-think";
const SEARCH: &str = "-search";

/// Что показывает `/models` и `GET /v1/models`. Варианты с `-search` принимаются,
/// но в список не входят: поиск нужен редко, а список удвоился бы.
pub const LISTED_MODELS: [&str; 4] = [
    "deepseek-chat",
    "deepseek-chat-think",
    "deepseek-expert",
    "deepseek-reasoner",
];

impl DeepseekMode {
    /// Строгий разбор: `None` — это не id DeepSeek вовсе.
    pub fn parse(model: &str) -> Option<Self> {
        let lower = model.trim().to_ascii_lowercase();
        let mut rest = lower.strip_prefix(PREFIX)?;
        let search = strip_suffix(&mut rest, SEARCH);
        let think = strip_suffix(&mut rest, THINK);
        let (model, thinking) = match rest {
            "chat" => (ModelKind::Instant, think),
            "expert" => (ModelKind::Expert, think),
            "reasoner" => (ModelKind::Expert, true),
            _ => return None,
        };
        Some(Self {
            model,
            thinking,
            search,
        })
    }

    /// Разбор для провайдера: незнакомое имя — Instant без флагов, как было
    /// до разбора (в конфиге мог остаться любой id).
    pub fn from_model(model: &str) -> Self {
        Self::parse(model).unwrap_or(Self {
            model: ModelKind::Instant,
            thinking: false,
            search: false,
        })
    }

    /// `model_type` для тела запроса. Модель треда задаётся при создании, поэтому
    /// Instant на продолжении не шлётся; Expert шлётся всегда, как и прежде.
    pub fn wire_model_type(&self, parent_message_id: Option<i64>) -> Option<&'static str> {
        match self.model {
            ModelKind::Expert => Some("expert"),
            ModelKind::Instant if parent_message_id.is_none() => Some("default"),
            ModelKind::Instant => None,
        }
    }
}

fn strip_suffix(rest: &mut &str, suffix: &str) -> bool {
    match rest.strip_suffix(suffix) {
        Some(stripped) => {
            *rest = stripped;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(model: ModelKind, thinking: bool, search: bool) -> Option<DeepseekMode> {
        Some(DeepseekMode {
            model,
            thinking,
            search,
        })
    }

    #[test]
    fn model_and_toggles_are_independent() {
        use ModelKind::*;
        assert_eq!(
            DeepseekMode::parse("deepseek-chat"),
            mode(Instant, false, false)
        );
        assert_eq!(
            DeepseekMode::parse("deepseek-chat-think"),
            mode(Instant, true, false)
        );
        assert_eq!(
            DeepseekMode::parse("deepseek-expert"),
            mode(Expert, false, false)
        );
        assert_eq!(
            DeepseekMode::parse("deepseek-expert-think"),
            mode(Expert, true, false)
        );
        assert_eq!(
            DeepseekMode::parse("DeepSeek-Chat-Search"),
            mode(Instant, false, true)
        );
        assert_eq!(
            DeepseekMode::parse("deepseek-expert-think-search"),
            mode(Expert, true, true)
        );
    }

    /// Старое имя продолжает значить то же, что раньше: Expert с DeepThink.
    #[test]
    fn reasoner_keeps_its_old_meaning() {
        assert_eq!(
            DeepseekMode::parse("deepseek-reasoner"),
            mode(ModelKind::Expert, true, false)
        );
    }

    #[test]
    fn foreign_and_misordered_ids_are_not_deepseek() {
        for id in [
            "gpt-4o",
            "deepseek-coder",
            "deepseek-chat-search-think",
            "chat",
        ] {
            assert_eq!(DeepseekMode::parse(id), None, "{id}");
        }
        assert_eq!(
            DeepseekMode::from_model("gpt-4o"),
            DeepseekMode::parse("deepseek-chat").unwrap()
        );
    }

    #[test]
    fn every_listed_model_parses() {
        for id in LISTED_MODELS {
            assert!(DeepseekMode::parse(id).is_some(), "{id}");
        }
    }

    #[test]
    fn instant_model_type_is_sent_only_on_a_new_thread() {
        let chat = DeepseekMode::from_model("deepseek-chat");
        assert_eq!(chat.wire_model_type(None), Some("default"));
        assert_eq!(chat.wire_model_type(Some(5)), None);
        let expert = DeepseekMode::from_model("deepseek-expert");
        assert_eq!(expert.wire_model_type(Some(5)), Some("expert"));
    }
}
