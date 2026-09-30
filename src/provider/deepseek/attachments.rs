//! Вложения DeepSeek: загрузить файл, дождаться разбора, отдать id в `ref_file_ids`.
//! Протокол снят с веб-клиента 2.5.0 (2026-09-30); id кешируются на все форки.

use super::DeepseekProvider;
use crate::debug_log;
use crate::error::{AppError, AppResult};
use crate::provider::types::{FileStatus, UploadedFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Как часто спрашивать о готовности. Файл на 2,4 МБ сервер разобрал за ~1 с.
const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// Дольше разбор не ждём: ход честнее уронить с причиной, чем молча висеть.
const PARSE_TIMEOUT: Duration = Duration::from_secs(180);
/// Столько сетевых сбоев подряд переживает опрос: `max_retries` по умолчанию
/// 0, и один обрыв иначе выбрасывал бы уже загруженный файл.
const POLL_ERRORS_TOLERATED: u32 = 3;
/// Больше не грузим: файл читается в память целиком. 100 МБ — предел
/// веб-клиента по вторичным источникам; сервером не подтверждён.
const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;

/// Типы из `accept` поля загрузки веб-клиента 2.5.0: документы, картинки и текст
/// (сюда он попадает, только если не в UTF-8). Архивы и программы сервер не разбирает.
const UPLOADABLE: &[&str] = &[
    "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "epub", "mobi", "png", "jpg", "jpeg",
    "jfif", "pjpeg", "pjp", "gif", "webp", "bmp", "dib", "tif", "tiff", "avif", "apng", "ico",
    "svg", "svgz", "psd", "tga", "jp2", "txt", "md", "csv", "tsv", "log", "json", "html", "htm",
    "xml", "yaml", "yml", "ini", "conf", "toml", "sql", "rs", "py", "js", "ts", "tsx", "jsx", "c",
    "h", "cpp", "hpp", "cs", "java", "kt", "go", "rb", "php", "sh", "bat", "ps1", "lua",
];

/// Разберёт ли сервер этот файл, если приложить его вложением.
pub(super) fn uploadable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| UPLOADABLE.contains(&ext.to_ascii_lowercase().as_str()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UploadKey {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// Итог приложения файлов одной отправки.
#[derive(Default)]
pub(super) struct Attached {
    pub(super) ready: Vec<ReadyFile>,
    /// «путь — причина» для файлов, которые приложить не вышло.
    pub(super) failed: Vec<String>,
}

impl Attached {
    /// Заметка модели о неприложенных файлах: пусть скажет человеку.
    pub(super) fn failure_note(&self) -> Option<String> {
        (!self.failed.is_empty()).then(|| {
            format!(
                "Эти файлы приложить не удалось, их содержимого у тебя нет — скажи об этом пользователю:
{}",
                self.failed.join("
")
            )
        })
    }
}

/// Готовый к ссылке файл.
#[derive(Debug, Clone)]
pub(super) struct ReadyFile {
    pub(super) id: String,
    /// Сколько токенов сервер насчитал файлу (у него они и тратятся).
    pub(super) tokens: u32,
}

#[derive(Default)]
pub(super) struct UploadCache(Mutex<HashMap<UploadKey, ReadyFile>>);

impl UploadCache {
    fn get(&self, key: &UploadKey) -> Option<ReadyFile> {
        self.0.lock().ok()?.get(key).cloned()
    }

    fn put(&self, key: UploadKey, file: ReadyFile) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(key, file);
        }
    }

    fn forget(&self, key: &UploadKey) {
        if let Ok(mut map) = self.0.lock() {
            map.remove(key);
        }
    }
}

/// Пока идёт долгий запрос (PoW, загрузка сотни МБ), раз в полминуты держать
/// таймер простоя потока (120 с) пустым куском.
async fn with_heartbeat<T>(
    work: impl std::future::Future<Output = T>,
    heartbeat: &mut (dyn FnMut() + Send),
) -> T {
    const EVERY: Duration = Duration::from_secs(30);
    tokio::pin!(work);
    loop {
        tokio::select! {
            done = &mut work => return done,
            _ = tokio::time::sleep(EVERY) => heartbeat(),
        }
    }
}

/// Что значит очередной ответ `fetch_files`.
#[derive(Debug, PartialEq, Eq)]
enum Parse {
    Ready(u32),
    Pending,
    Rejected(String),
}

fn parse_state(file: &UploadedFile) -> Parse {
    let code = match &file.error_code {
        Some(serde_json::Value::String(code)) if !code.is_empty() => Some(code.clone()),
        Some(serde_json::Value::Number(code)) if code.as_i64() != Some(0) => Some(code.to_string()),
        _ => None,
    };
    if let Some(code) = code {
        return Parse::Rejected(format!("сервер отклонил файл: {code}"));
    }
    match file.status {
        FileStatus::Success => {
            let tokens = file.token_usage.unwrap_or(0).clamp(0, u32::MAX as i64) as u32;
            Parse::Ready(tokens)
        }
        FileStatus::Failed => Parse::Rejected("сервер не смог разобрать файл".to_string()),
        FileStatus::Pending | FileStatus::Unknown => Parse::Pending,
    }
}

impl DeepseekProvider {
    /// Приложить файлы хвоста. Неудача одного файла не роняет ход: сообщение
    /// с ним осталось бы в хвосте и валило бы каждую следующую отправку.
    pub(super) async fn attach_files(
        &self,
        paths: &[String],
        heartbeat: &mut (dyn FnMut() + Send),
    ) -> Attached {
        let mut attached = Attached::default();
        for raw in paths {
            match self.attach_file(Path::new(raw), heartbeat).await {
                Ok(file) => attached.ready.push(file),
                Err(error) => {
                    debug_log::log("file.attach_failed", format!("{raw}: {error}"));
                    attached.failed.push(format!("{raw} — {error}"));
                }
            }
        }
        attached
    }

    async fn attach_file(
        &self,
        path: &Path,
        heartbeat: &mut (dyn FnMut() + Send),
    ) -> AppResult<ReadyFile> {
        let canonical = tokio::fs::canonicalize(path).await.map_err(AppError::Io)?;
        let meta = tokio::fs::metadata(&canonical)
            .await
            .map_err(AppError::Io)?;
        if meta.len() > MAX_UPLOAD_BYTES {
            return Err(AppError::Provider(format!(
                "файл больше {} МБ",
                MAX_UPLOAD_BYTES / 1024 / 1024
            )));
        }
        let key = UploadKey {
            path: canonical,
            size: meta.len(),
            modified: meta.modified().ok(),
        };
        if let Some(hit) = self.uploads.get(&key) {
            if self.still_ready(&hit.id).await {
                debug_log::log(
                    "file.cache_hit",
                    format!("{} id={}", path.display(), hit.id),
                );
                return Ok(hit);
            }
            debug_log::log(
                "file.cache_stale",
                format!("{} id={}", path.display(), hit.id),
            );
            self.uploads.forget(&key);
        }
        let uploaded = with_heartbeat(self.upload_file(&key.path), heartbeat).await?;
        debug_log::log(
            "file.uploaded",
            format!(
                "{} id={} status={:?}",
                path.display(),
                uploaded.id,
                uploaded.status
            ),
        );
        let ready = self.wait_parsed(uploaded.id, heartbeat).await?;
        self.uploads.put(key, ready.clone());
        Ok(ready)
    }

    /// Жив ли на сервере id из кеша. Любое сомнение — «нет», загрузим заново.
    async fn still_ready(&self, id: &str) -> bool {
        match self.fetch_uploaded_files(&[id.to_string()]).await {
            Ok(files) => files
                .iter()
                .find(|f| f.id == id)
                .is_some_and(|f| matches!(parse_state(f), Parse::Ready(_))),
            Err(_) => false,
        }
    }

    /// Ждать `SUCCESS`. Отказ сервера уходит пользователю с причиной;
    /// незнакомый статус считаем промежуточным, но пишем в лог.
    async fn wait_parsed(
        &self,
        id: String,
        heartbeat: &mut (dyn FnMut() + Send),
    ) -> AppResult<ReadyFile> {
        let started = Instant::now();
        let mut errors = 0;
        loop {
            match self.fetch_uploaded_files(std::slice::from_ref(&id)).await {
                Ok(files) => {
                    errors = 0;
                    let file = files
                        .iter()
                        .find(|f| f.id == id)
                        .ok_or_else(|| AppError::Provider(format!("сервер не знает файл {id}")))?;
                    if file.status == FileStatus::Unknown {
                        debug_log::log("file.unknown_status", format!("id={id}"));
                    }
                    match parse_state(file) {
                        Parse::Ready(tokens) => {
                            debug_log::log("file.ready", format!("id={id} tokens={tokens}"));
                            return Ok(ReadyFile { id, tokens });
                        }
                        Parse::Rejected(reason) => return Err(AppError::Provider(reason)),
                        Parse::Pending => {}
                    }
                }
                Err(error) if errors < POLL_ERRORS_TOLERATED => {
                    errors += 1;
                    debug_log::log(
                        "file.poll_error",
                        format!("id={id} attempt={errors} {error}"),
                    );
                }
                Err(error) => return Err(error),
            }
            if started.elapsed() > PARSE_TIMEOUT {
                return Err(AppError::Provider(format!(
                    "сервер разбирает файл дольше {} с",
                    PARSE_TIMEOUT.as_secs()
                )));
            }
            // Пустой кусок держит таймер простоя потока (120 с), пока сервер разбирает файл.
            heartbeat();
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(json: &str) -> UploadedFile {
        serde_json::from_str(json).unwrap()
    }

    /// Ответ снят с живого сервера 2026-09-30: дробные метки времени и
    /// `model_kind: VISION`, на которых ломался прежний разбор.
    #[test]
    fn the_live_fetch_files_shape_parses() {
        let live = r#"{"id":"file-1","status":"SUCCESS","file_name":"a.md","from_share":false,
            "file_size":286,"model_kind":"VISION","token_usage":80,"error_code":null,
            "inserted_at":1790741885.457,"updated_at":1790741886.0,"signed_path":"/f","is_image":false,"audit_result":null}"#;
        assert_eq!(parse_state(&file(live)), Parse::Ready(80));
    }

    #[test]
    fn statuses_map_to_ready_pending_or_rejected() {
        assert_eq!(
            parse_state(&file(r#"{"id":"f","status":"PENDING"}"#)),
            Parse::Pending
        );
        assert_eq!(
            parse_state(&file(r#"{"id":"f","status":"PARSING"}"#)),
            Parse::Pending
        );
        assert!(matches!(
            parse_state(&file(r#"{"id":"f","status":"FAILED"}"#)),
            Parse::Rejected(_)
        ));
        let with_code = file(r#"{"id":"f","status":"PENDING","error_code":"FILE_TOO_LARGE"}"#);
        assert_eq!(
            parse_state(&with_code),
            Parse::Rejected("сервер отклонил файл: FILE_TOO_LARGE".to_string())
        );
        let zero = file(r#"{"id":"f","status":"SUCCESS","error_code":0,"token_usage":5}"#);
        assert_eq!(parse_state(&zero), Parse::Ready(5), "0 is no error");
        // Так сервер отвечает на битый PDF: код числом, и это отказ, а не сбой сети.
        let numeric = file(r#"{"id":"f","status":"PENDING","error_code":40000}"#);
        assert_eq!(
            parse_state(&numeric),
            Parse::Rejected("сервер отклонил файл: 40000".to_string())
        );
    }

    #[test]
    fn documents_images_and_odd_encoded_text_upload_but_archives_do_not() {
        for ok in ["a.PDF", "b.docx", "c.png", "notes.txt"] {
            assert!(uploadable(Path::new(ok)), "{ok}");
        }
        for no in ["build.zip", "app.exe", "db.sqlite", "noext"] {
            assert!(!uploadable(Path::new(no)), "{no}");
        }
    }
}
