use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DeepseekCompletionResponse {
    pub id: String,
    pub choices: Vec<DeepseekChoice>,
    pub usage: Option<DeepseekUsage>,
}

#[derive(Debug, Deserialize)]
pub struct DeepseekChoice {
    pub index: u32,
    pub message: Option<DeepseekMessage>,
    pub delta: Option<DeepseekDelta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeepseekMessage {
    pub role: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeepseekDelta {
    pub role: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeepseekUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Deserialize)]
pub struct DeepseekStreamEvent {
    pub data: String,
}

// DeepSeek web API types (reverse-engineered)
#[derive(Debug, Deserialize)]
pub struct PowChallengeResponse {
    pub req_id: String,
    pub algorithm: String,
    pub challenge: PowChallenge,
}

#[derive(Debug, Deserialize)]
pub struct PowChallenge {
    pub seed: u64,
    pub challenge: String,
    pub difficulty: u32,
}
