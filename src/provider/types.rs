//! Wire-format request/response modeling for the DeepSeek web API. Only a
//! small slice (the streaming completion path, driven from `deepseek.rs`'s
//! `impl LLMProvider`) is on the hot path this TUI actually exercises; the
//! rest models the broader reverse-engineered API (session CRUD, sharing,
//! search, file upload, user settings) for the unused-but-kept client
//! methods in `deepseek.rs`. See the `#[allow(dead_code)]` on that impl
//! block for why it's suppressed here rather than deleted.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ─── API Response Envelope ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub code: i64,
    pub msg: String,
    pub data: ApiData<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiData<T> {
    pub biz_code: i64,
    pub biz_msg: String,
    pub biz_data: T,
}

// ─── Pagination ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedResponse<T> {
    pub items: Vec<T>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LteCursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

// ─── Chat Session ──────────────────────────────────────────────

pub type ChatSessionId = String;
pub type MessageId = i64;
pub type FileId = String;
pub type ShareId = String;

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TitleType {
    WIP,
    SYSTEM,
    USER,
}

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MessageStatus {
    WIP,
    FINISHED,
    ABORTED,
}

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FragmentType {
    REQUEST,
    RESPONSE,
    THINK,
}

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChatSessionStatus {
    WIP,
    FINISHED,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ModelType {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "expert")]
    Expert,
    #[serde(rename = "vision")]
    Vision,
}

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConversationMode {
    DEFAULT,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: ChatSessionId,
    pub seq_id: i64,
    pub agent: String,
    pub model_type: ModelType,
    #[serde(default)]
    pub title: Option<String>,
    pub title_type: TitleType,
    pub version: i64,
    #[serde(default)]
    pub current_message_id: Option<MessageId>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub is_empty: Option<bool>,
    pub inserted_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionData {
    pub chat_session: ChatSession,
    pub ttl_seconds: i64,
}

// ─── Messages ──────────────────────────────────────────────────

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MessageRole {
    USER,
    ASSISTANT,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackType {
    Like,
    Dislike,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageReference {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fragment {
    pub id: i64,
    #[serde(rename = "type")]
    pub fragment_type: FragmentType,
    pub content: String,
    #[serde(default)]
    pub elapsed_secs: Option<f64>,
    #[serde(default)]
    pub references: Vec<MessageReference>,
    pub stage_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekMessage {
    pub message_id: MessageId,
    #[serde(default)]
    pub parent_id: Option<MessageId>,
    pub model: String,
    pub role: MessageRole,
    pub thinking_enabled: bool,
    pub ban_edit: bool,
    pub ban_regenerate: bool,
    pub status: MessageStatus,
    #[serde(default)]
    pub incomplete_message: Option<String>,
    pub accumulated_token_usage: i64,
    #[serde(default)]
    pub feedback: Option<FeedbackType>,
    pub inserted_at: i64,
    pub search_enabled: bool,
    pub fragments: Vec<Fragment>,
    pub conversation_mode: ConversationMode,
    pub has_pending_fragment: bool,
    pub auto_continue: bool,
    pub search_triggered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryMessagesData {
    pub chat_session: ChatSession,
    pub chat_messages: Vec<DeepSeekMessage>,
    pub cache_control: String,
    pub cache_reset_at: i64,
}

// ─── SSE Events ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionReadyEvent {
    pub request_message_id: MessageId,
    pub response_message_id: MessageId,
    pub model_type: ModelType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionUpdateSessionEvent {
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionTitleEvent {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionCloseEvent {
    pub click_behavior: String,
    pub auto_resume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponseEvent {
    pub response: DeepSeekMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionContentAppendEvent {
    pub p: String,
    pub o: String,
    pub v: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionTokenDeltaEvent {
    pub v: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionFragmentAppendEvent {
    pub p: String,
    pub o: String,
    pub v: Vec<Fragment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionFieldSetEvent {
    pub p: String,
    pub o: String,
    pub v: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionBatchPatch {
    pub p: String,
    pub v: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionBatchEvent {
    pub p: String,
    pub o: String,
    pub v: Vec<CompletionBatchPatch>,
}

// ─── Completion Request ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionAction {
    Regenerate,
    Edit,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekCompletionRequest {
    pub chat_session_id: ChatSessionId,
    #[serde(default)]
    pub parent_message_id: Option<MessageId>,
    #[serde(default)]
    pub model_type: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub ref_file_ids: Vec<FileId>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    #[serde(default)]
    pub action: Option<CompletionAction>,
    #[serde(default)]
    pub preempt: bool,
}

// ─── PoW ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowChallengeRequest {
    pub target_path: String,
}

// ─── Message Actions ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageFeedbackRequest {
    pub chat_session_id: ChatSessionId,
    pub message_id: MessageId,
    pub feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopStreamRequest {
    pub chat_session_id: ChatSessionId,
    pub response_message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeStreamRequest {
    pub chat_session_id: ChatSessionId,
    pub response_message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegenerateRequest {
    pub chat_session_id: ChatSessionId,
    pub parent_message_id: MessageId,
    #[serde(default)]
    pub model_type: Option<String>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
    #[serde(default)]
    pub ref_file_ids: Vec<FileId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueRequest {
    pub chat_session_id: ChatSessionId,
    pub response_message_id: MessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditMessageRequest {
    pub chat_session_id: ChatSessionId,
    pub message_id: MessageId,
    pub prompt: String,
    #[serde(default)]
    pub ref_file_ids: Vec<FileId>,
    pub thinking_enabled: bool,
    pub search_enabled: bool,
}

// ─── Session Management ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTitleRequest {
    pub chat_session_id: ChatSessionId,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePinnedRequest {
    pub chat_session_id: ChatSessionId,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSessionRequest {
    pub chat_session_id: ChatSessionId,
}

// ─── File ──────────────────────────────────────────────────────

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileStatus {
    PENDING,
    SUCCESS,
    FAILED,
}

// wire format — names must match the API JSON (SCREAMING_SNAKE_CASE via
// `rename_all`); renaming the variants would change what's serialized.
#[expect(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ModelKind {
    NORMAL,
    R1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedFile {
    pub id: FileId,
    pub status: FileStatus,
    pub file_name: String,
    pub from_share: bool,
    pub file_size: i64,
    pub model_kind: ModelKind,
    #[serde(default)]
    pub token_usage: Option<i64>,
    #[serde(default)]
    pub error_code: Option<String>,
    pub inserted_at: i64,
    pub updated_at: i64,
    pub is_image: bool,
    #[serde(default)]
    pub audit_result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedFile {
    pub id: FileId,
    pub status: FileStatus,
    pub file_name: String,
    pub from_share: bool,
    pub file_size: i64,
    pub model_kind: ModelKind,
    #[serde(default)]
    pub token_usage: Option<i64>,
    #[serde(default)]
    pub error_code: Option<String>,
    pub inserted_at: i64,
    pub updated_at: i64,
    pub is_image: bool,
    #[serde(default)]
    pub audit_result: Option<serde_json::Value>,
    pub signed_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchFilesData {
    pub files: Vec<FetchedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkFileTaskRequest {
    pub file_id: FileId,
}

// ─── Share ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateShareRequest {
    pub chat_session_id: ChatSessionId,
    pub message_ids: Vec<MessageId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateShareData {
    pub share_id: ShareId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSummary {
    pub share_id: ShareId,
    pub hint: String,
    pub created_at: i64,
    pub chat_session_id: ChatSessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareListData {
    pub shares: Vec<ShareSummary>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareContentData {
    pub title: String,
    pub messages: Vec<DeepSeekMessage>,
    pub model_type: ModelType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteShareRequest {
    pub share_id: ShareId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkShareRequest {
    pub share_id: ShareId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkShareData {
    pub chat_session_id: ChatSessionId,
}

// ─── Search ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPrepareRequest {
    #[serde(default)]
    pub chat_session_id: Option<ChatSessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexQueryRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexQueryHit {
    pub chat_session_id: ChatSessionId,
    pub message_id: MessageId,
    pub snippet: String,
    #[serde(default)]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexQueryData {
    pub hits: Vec<IndexQueryHit>,
    pub has_more: bool,
}

// ─── User ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserChatSettings {
    pub is_muted: i64,
    #[serde(default)]
    pub mute_until: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepSeekUser {
    pub id: String,
    pub token: String,
    pub email: String,
    pub mobile_number: String,
    pub area_code: String,
    pub status: i64,
    #[serde(default)]
    pub id_profile: Option<String>,
    #[serde(default)]
    pub id_profiles: Vec<String>,
    pub chat: UserChatSettings,
    pub has_legacy_chat_history: bool,
    pub need_birthday: bool,
}

