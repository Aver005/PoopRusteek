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

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("Task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("{0}")]
    Custom(String),
}

impl From<color_eyre::Report> for AppError {
    fn from(err: color_eyre::Report) -> Self {
        AppError::Custom(err.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
