//! The DeepSeek web API exposes a much larger surface than this TUI client
//! currently drives — session CRUD, sharing, search, file upload, user
//! settings. It is modeled here (methods + the request/response types in
//! `types.rs`) for parity with the reverse-engineered API and future feature
//! work. The one live remote-session method actually used by the
//! `LLMProvider` impl (`delete_remote_session`, called from
//! `discard_remote_session`) lives in this file too since it shares the same
//! endpoint-URL neighborhood, but the rest of the surface is
//! `#[expect(dead_code)]` — suppressed as a block rather than deleted: unlike
//! the verified-dead code removed elsewhere, this is intentionally-kept API
//! modeling, not litter.

use super::DeepseekProvider;
use super::http::api_refusal;
use crate::error::AppResult;
use crate::provider::types;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

// Session management
const FETCH_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
const DELETE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
const DELETE_ALL_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete_all";
const UPDATE_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
const UPDATE_PINNED_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_pinned";

// Message actions
const MESSAGE_FEEDBACK_URL: &str = "https://chat.deepseek.com/api/v0/chat/message_feedback";
const EDIT_MESSAGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/edit_message";
const REGENERATE_URL: &str = "https://chat.deepseek.com/api/v0/chat/regenerate";
const STOP_STREAM_URL: &str = "https://chat.deepseek.com/api/v0/chat/stop_stream";
const RESUME_STREAM_URL: &str = "https://chat.deepseek.com/api/v0/chat/resume_stream";

// File
const UPLOAD_FILE_URL: &str = "https://chat.deepseek.com/api/v0/file/upload_file";
const FETCH_FILES_URL: &str = "https://chat.deepseek.com/api/v0/file/fetch_files";
const FORK_FILE_TASK_URL: &str = "https://chat.deepseek.com/api/v0/file/fork_file_task";

// Share
const CREATE_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/create";
const LIST_SHARES_URL: &str = "https://chat.deepseek.com/api/v0/share/list";
const SHARE_CONTENT_URL: &str = "https://chat.deepseek.com/api/v0/share/content";
const DELETE_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/delete";
const FORK_SHARE_URL: &str = "https://chat.deepseek.com/api/v0/share/fork";

// Search
const INDEX_PREPARE_URL: &str = "https://chat.deepseek.com/api/v0/index/prepare";
const INDEX_QUERY_URL: &str = "https://chat.deepseek.com/api/v0/index/query";

// User
const CURRENT_USER_URL: &str = "https://chat.deepseek.com/api/v0/users/current";
const LOGOUT_ALL_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/users/logout_all_sessions";
const SET_BIRTHDAY_URL: &str = "https://chat.deepseek.com/api/v0/users/set_birthday";

// Client settings & telemetry
const CLIENT_SETTINGS_URL: &str = "https://chat.deepseek.com/api/v0/client/settings";
const CLIENT_SETTINGS_REPORT_URL: &str = "https://chat.deepseek.com/api/v0/client/settings/report";

impl DeepseekProvider {
    /// POST `url` with `body`, parse the standard `ApiResponse<T>` envelope,
    /// and return `.data.biz_data`. Only fits endpoints whose success path is
    /// exactly that shape — several wrappers below differ (raw `Response`,
    /// no body to parse, or a nested field other than `biz_data`) and use
    /// `post_void` or call `send_json_request` directly rather than being
    /// forced onto this helper.
    async fn post_biz<T: DeserializeOwned>(
        &self,
        action: &str,
        url: &str,
        body: Value,
    ) -> AppResult<T> {
        let headers = self.auth_headers()?;
        let response = self.send_json_request(action, url, &headers, &body).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response(action, response, action).await);
        }
        biz_data(action, Self::read_json(action, response, action).await?)
    }

    /// POST `url` with `body` using plain auth headers and discard the
    /// response body on success. Fits the fire-and-forget endpoints whose
    /// only interesting output is the error path; `error_context` is the
    /// human-readable label passed to `read_error_response`.
    async fn post_void(
        &self,
        action: &str,
        url: &str,
        body: Value,
        error_context: &str,
    ) -> AppResult<()> {
        let headers = self.auth_headers()?;
        self.post_void_with_headers(action, url, headers, body, error_context)
            .await
    }

    /// `post_void` variant taking caller-built headers — used by the PoW
    /// endpoints, which must solve the challenge and attach
    /// `x-ds-pow-response` before the request is sent.
    async fn post_void_with_headers(
        &self,
        action: &str,
        url: &str,
        headers: HeaderMap,
        body: Value,
        error_context: &str,
    ) -> AppResult<()> {
        let response = self.send_json_request(action, url, &headers, &body).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response(action, response, error_context).await);
        }
        Ok(())
    }

    /// GET `url`, parse the standard `ApiResponse<T>` envelope, and return
    /// `.data.biz_data`. See `post_biz` for why not every GET wrapper uses
    /// this (e.g. `fetch_uploaded_files` unwraps a nested `.files` field,
    /// `get_client_settings` returns the raw un-enveloped JSON).
    async fn get_biz<T: DeserializeOwned>(&self, action: &str, url: &str) -> AppResult<T> {
        let headers = self.auth_headers()?;
        let response = self.send_get_request(action, url, &headers).await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response(action, response, action).await);
        }
        biz_data(action, Self::read_json(action, response, action).await?)
    }
}

/// MIME-тип части multipart, как его ставит браузер. Незнакомое — поток байт:
/// сервер разбирает файл по содержимому и имени.
fn mime_type(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "doc" => "application/msword",
        "ppt" => "application/vnd.ms-powerpoint",
        "xls" => "application/vnd.ms-excel",
        "epub" => "application/epub+zip",
        "md" => "text/markdown",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        _ => "application/octet-stream",
    }
}

/// `data.biz_data` конверта, разобранный в `T`.
///
/// Отдельная функция, а не `ApiResponse<T>` целиком, потому что отказ обязан
/// читаться **до** разбора полезной нагрузки: при `biz_code != 0` она равна
/// `null`, и типизированный разбор ругался бы на форму вместо того, чтобы
/// назвать причину («invalid chat session id»).
fn biz_data<T: DeserializeOwned>(action: &str, payload: Value) -> AppResult<T> {
    if let Some(refusal) = api_refusal(&payload) {
        return Err(crate::error::AppError::Provider(format!(
            "{action}: {}",
            refusal.message()
        )));
    }
    serde_json::from_value(payload["data"]["biz_data"].clone()).map_err(|error| {
        crate::error::AppError::Provider(format!("{action}: unexpected response shape: {error}"))
    })
}

/// The `fetch_page` response's `biz_data` — the session array arrives under
/// `chat_sessions`, not a generic `items` field.
#[derive(Debug, serde::Deserialize)]
struct FetchSessionsBizData {
    chat_sessions: Vec<types::ChatSession>,
}

impl DeepseekProvider {
    // ─── Session Management ────────────────────────────────────

    /// Delete a remote session.
    pub async fn delete_remote_session(&self, session_id: &str) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id });
        self.post_void(
            "session.delete",
            DELETE_SESSION_URL,
            body,
            "Delete session failed",
        )
        .await
    }

    /// Fetch the account's remote session list (first page).
    pub async fn fetch_remote_sessions(
        &self,
        pinned: Option<bool>,
    ) -> AppResult<Vec<types::ChatSession>> {
        let mut url = FETCH_SESSIONS_URL.to_string();
        if let Some(p) = pinned {
            url.push_str(&format!("?lte_cursor.pinned={p}"));
        }
        let data: FetchSessionsBizData = self.get_biz("sessions.fetch", &url).await?;
        Ok(data.chat_sessions)
    }
}

// See the module doc comment for why this block is suppressed rather than
// pruned: it's intentionally-kept API modeling for future feature work, not
// verified-dead litter.
#[expect(dead_code)]
impl DeepseekProvider {
    /// Delete all remote sessions.
    pub async fn delete_all_remote_sessions(&self) -> AppResult<()> {
        let body = json!({});
        self.post_void(
            "session.delete_all",
            DELETE_ALL_SESSIONS_URL,
            body,
            "Delete all sessions failed",
        )
        .await
    }

    /// Rename a remote session.
    pub async fn rename_remote_session(&self, session_id: &str, title: &str) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id, "title": title });
        self.post_void(
            "session.rename",
            UPDATE_TITLE_URL,
            body,
            "Rename session failed",
        )
        .await
    }

    /// Pin or unpin a remote session.
    pub async fn pin_remote_session(&self, session_id: &str, pinned: bool) -> AppResult<()> {
        let body = json!({ "chat_session_id": session_id, "pinned": pinned });
        self.post_void("session.pin", UPDATE_PINNED_URL, body, "Pin session failed")
            .await
    }

    // ─── Message Actions ───────────────────────────────────────

    /// Send like/dislike feedback for a message.
    pub async fn send_message_feedback(
        &self,
        session_id: &str,
        message_id: i64,
        feedback: &str,
    ) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "message_id": message_id,
            "feedback": feedback,
        });
        self.post_void(
            "message.feedback",
            MESSAGE_FEEDBACK_URL,
            body,
            "Feedback failed",
        )
        .await
    }

    /// Edit a message (requires PoW).
    pub async fn edit_message(
        &self,
        session_id: &str,
        message_id: i64,
        prompt: &str,
        thinking_enabled: bool,
        search_enabled: bool,
    ) -> AppResult<()> {
        let headers = self.pow_headers(EDIT_MESSAGE_URL).await?;
        let body = json!({
            "chat_session_id": session_id,
            "message_id": message_id,
            "prompt": prompt,
            "ref_file_ids": [],
            "thinking_enabled": thinking_enabled,
            "search_enabled": search_enabled,
        });
        self.post_void_with_headers(
            "message.edit",
            EDIT_MESSAGE_URL,
            headers,
            body,
            "Edit message failed",
        )
        .await
    }

    /// Regenerate the last assistant response (requires PoW).
    pub async fn regenerate_message(
        &self,
        session_id: &str,
        parent_message_id: i64,
        thinking_enabled: bool,
        search_enabled: bool,
    ) -> AppResult<()> {
        let headers = self.pow_headers(REGENERATE_URL).await?;
        let body = json!({
            "chat_session_id": session_id,
            "parent_message_id": parent_message_id,
            "model_type": null,
            "thinking_enabled": thinking_enabled,
            "search_enabled": search_enabled,
            "ref_file_ids": [],
        });
        self.post_void_with_headers(
            "message.regenerate",
            REGENERATE_URL,
            headers,
            body,
            "Regenerate failed",
        )
        .await
    }

    /// Stop an active stream.
    pub async fn stop_stream(&self, session_id: &str, response_message_id: i64) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "response_message_id": response_message_id,
        });
        self.post_void(
            "message.stop_stream",
            STOP_STREAM_URL,
            body,
            "Stop stream failed",
        )
        .await
    }

    /// Resume a stopped stream.
    pub async fn resume_stream(&self, session_id: &str, response_message_id: i64) -> AppResult<()> {
        let body = json!({
            "chat_session_id": session_id,
            "response_message_id": response_message_id,
        });
        self.post_void(
            "message.resume_stream",
            RESUME_STREAM_URL,
            body,
            "Resume stream failed",
        )
        .await
    }

    // ─── File Operations ───────────────────────────────────────

    /// Загрузка как у веб-клиента 2.5.0 (снято 2026-09-30): PoW на путь загрузки,
    /// `x-file-size`, поле `file`. Ответ — `PENDING`, готовность ждёт `fetch_files`.
    pub async fn upload_file(&self, path: &std::path::Path) -> AppResult<types::UploadedFile> {
        use reqwest::multipart;

        let bytes = tokio::fs::read(path)
            .await
            .map_err(crate::error::AppError::Io)?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();

        let mut headers = self.pow_headers(UPLOAD_FILE_URL).await?;
        // Общий `application/json` затёр бы `multipart/form-data; boundary=…`,
        // который ставит сам reqwest: сервер отвечал 400 «Invalid boundary».
        headers.remove(reqwest::header::CONTENT_TYPE);
        headers.insert("x-thinking-enabled", HeaderValue::from_static("0"));
        headers.insert("x-model-type", HeaderValue::from_static("default"));
        headers.insert("x-file-size", HeaderValue::from(bytes.len() as u64));

        // «Пульс» держит поток живым, поэтому заглохшая загрузка без своего
        // предела висела бы вечно: минута плюс секунда на каждые 256 КБ.
        let timeout = std::time::Duration::from_secs(60 + bytes.len() as u64 / (256 * 1024));
        let file_part = multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(mime_type(path))
            .map_err(|e| crate::error::AppError::Custom(e.to_string()))?;
        let form = multipart::Form::new().part("file", file_part);

        self.enforce_rate_limit().await;
        let response = self
            .client
            .post(UPLOAD_FILE_URL)
            .headers(headers)
            .multipart(form)
            .timeout(timeout)
            .send()
            .await
            .map_err(crate::error::AppError::Http)?;
        if !response.status().is_success() {
            return Err(
                Self::read_error_response("file.upload", response, "File upload failed").await,
            );
        }
        let payload = Self::read_json("file.upload", response, "File upload failed").await?;
        biz_data("file.upload", payload)
    }

    /// Состояние загруженных файлов по их id.
    pub async fn fetch_uploaded_files(
        &self,
        file_ids: &[String],
    ) -> AppResult<Vec<types::UploadedFile>> {
        let query: Vec<String> = file_ids
            .iter()
            .map(|id| format!("file_ids={}", urlencoding(id)))
            .collect();
        let url = format!("{FETCH_FILES_URL}?{}", query.join("&"));
        let data: types::FetchFilesData = self.get_biz("file.fetch", &url).await?;
        Ok(data.files)
    }

    /// Fork a file task (re-process a file).
    pub async fn fork_file_task(&self, file_id: &str) -> AppResult<types::UploadedFile> {
        let body = json!({ "file_id": file_id });
        self.post_biz("file.fork", FORK_FILE_TASK_URL, body).await
    }

    // ─── Share Operations ──────────────────────────────────────

    /// Create a share link for messages in a session.
    pub async fn create_share(
        &self,
        session_id: &str,
        message_ids: &[i64],
    ) -> AppResult<types::CreateShareData> {
        let body = json!({
            "chat_session_id": session_id,
            "message_ids": message_ids,
        });
        self.post_biz("share.create", CREATE_SHARE_URL, body).await
    }

    /// List shares.
    pub async fn list_shares(&self, count: i64) -> AppResult<types::ShareListData> {
        let url = format!("{LIST_SHARES_URL}?count={count}");
        self.get_biz("share.list", &url).await
    }

    /// Get share content by share ID.
    pub async fn get_share_content(&self, share_id: &str) -> AppResult<types::ShareContentData> {
        let url = format!("{SHARE_CONTENT_URL}?share_id={share_id}");
        self.get_biz("share.content", &url).await
    }

    /// Delete a share.
    pub async fn delete_share(&self, share_id: &str) -> AppResult<()> {
        let body = json!({ "share_id": share_id });
        self.post_void(
            "share.delete",
            DELETE_SHARE_URL,
            body,
            "Delete share failed",
        )
        .await
    }

    /// Fork a shared conversation into a new session.
    pub async fn fork_share(&self, share_id: &str) -> AppResult<types::ForkShareData> {
        let body = json!({ "share_id": share_id });
        self.post_biz("share.fork", FORK_SHARE_URL, body).await
    }

    // ─── Search ────────────────────────────────────────────────

    /// Prepare the search index for a session (or all sessions).
    pub async fn prepare_search_index(&self, session_id: Option<&str>) -> AppResult<()> {
        let body = match session_id {
            Some(sid) => json!({ "chat_session_id": sid }),
            None => json!({}),
        };
        self.post_void(
            "index.prepare",
            INDEX_PREPARE_URL,
            body,
            "Prepare index failed",
        )
        .await
    }

    /// Query the conversation search index.
    pub async fn query_search_index(
        &self,
        query: &str,
        limit: Option<i64>,
    ) -> AppResult<types::IndexQueryData> {
        let body = json!({
            "query": query,
            "limit": limit,
        });
        self.post_biz("index.query", INDEX_QUERY_URL, body).await
    }

    // ─── User ──────────────────────────────────────────────────

    /// Get the current authenticated user's information.
    pub async fn get_current_user(&self) -> AppResult<types::DeepSeekUser> {
        self.get_biz("user.current", CURRENT_USER_URL).await
    }

    /// Logout all active sessions.
    pub async fn logout_all_sessions(&self) -> AppResult<()> {
        let body = json!({});
        self.post_void(
            "user.logout_all",
            LOGOUT_ALL_SESSIONS_URL,
            body,
            "Logout all failed",
        )
        .await
    }

    /// Set user birthday.
    pub async fn set_birthday(&self, birthday: &str) -> AppResult<()> {
        let body = json!({ "birthday": birthday });
        self.post_void(
            "user.set_birthday",
            SET_BIRTHDAY_URL,
            body,
            "Set birthday failed",
        )
        .await
    }

    // ─── Client Settings ──────────────────────────────────────

    /// Fetch client settings for a given scope.
    pub async fn get_client_settings(
        &self,
        did: &str,
        scope: &str,
    ) -> AppResult<serde_json::Value> {
        let url = format!(
            "{CLIENT_SETTINGS_URL}?did={}&scope={}",
            urlencoding(did),
            urlencoding(scope)
        );
        let headers = self.auth_headers()?;
        let response = self
            .send_get_request("client.settings", &url, &headers)
            .await?;
        if !response.status().is_success() {
            return Err(Self::read_error_response(
                "client.settings",
                response,
                "Get client settings failed",
            )
            .await);
        }
        let payload: Value = response.json().await?;
        Ok(payload)
    }

    /// Report client settings.
    pub async fn report_client_settings(
        &self,
        settings_ids: &[i64],
        did: &str,
        sso_id: &str,
    ) -> AppResult<()> {
        let body = json!({
            "settings_ids": settings_ids,
            "did": did,
            "sso_id": sso_id,
        });
        self.post_void(
            "client.settings.report",
            CLIENT_SETTINGS_REPORT_URL,
            body,
            "Report settings failed",
        )
        .await
    }
}

/// Простое URL-кодирование параметров запроса.
fn urlencoding(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::mime_type;
    use std::path::Path;

    #[test]
    fn mime_follows_the_extension_case_insensitively() {
        assert_eq!(mime_type(Path::new("a.PDF")), "application/pdf");
        assert_eq!(mime_type(Path::new("photo.jpeg")), "image/jpeg");
        assert_eq!(mime_type(Path::new("blob.bin")), "application/octet-stream");
        assert_eq!(mime_type(Path::new("noext")), "application/octet-stream");
    }
}
