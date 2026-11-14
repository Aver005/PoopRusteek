use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Provider error: {0}")]
    Provider(String),

    /// The provider said "too many requests" — worth waiting and retrying,
    /// unlike [`AppError::Provider`]. Separate variant so the retry decision
    /// is made on the error's meaning rather than by matching on its text.
    #[error("Rate limited: {0}")]
    RateLimited(String),

    /// Отказ апстрима с классом, `Retry-After` и заголовками лимитов.
    /// Текст тот же, что был у `Provider("{status}: {message}")`.
    #[error("Provider error: {0}")]
    Upstream(Box<crate::provider::failure::ProviderFailure>),

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("MCP server requires authorization")]
    McpAuthRequired { www_authenticate: Option<String> },

    #[error("Task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("{0}")]
    Custom(String),
}

impl AppError {
    /// Класс отказа, если он известен по типу, а не по тексту.
    pub fn failure_class(&self) -> Option<crate::provider::failure::FailureClass> {
        use crate::provider::failure::FailureClass;
        match self {
            Self::Upstream(failure) => Some(failure.class.clone()),
            Self::RateLimited(_) => Some(FailureClass::RateLimit { retry_after: None }),
            Self::Http(error) if error.is_timeout() => Some(FailureClass::Timeout),
            Self::Http(_) => Some(FailureClass::Network),
            Self::Json(_) => Some(FailureClass::Protocol),
            _ => None,
        }
    }

    /// Подсказка «через сколько повторить», если апстрим её назвал.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Upstream(failure) => failure.retry_after(),
            _ => None,
        }
    }
}

impl From<color_eyre::Report> for AppError {
    fn from(err: color_eyre::Report) -> Self {
        AppError::Custom(err.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
