//! Запуск процесса CLI: дерево гасится целиком, сторож простоя, отмена убивает сразу.
//! Вывод режется по байту `\n` и декодируется построчно: оба CLI пишут UTF-8 JSONL (инвариант 4).

use super::launch::Launch;
use crate::tools::background::ProcessTree;
use crate::util::{AbortOnDrop, decode_process_output};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Дольше этого строку не копим: рваный вывод не должен съесть память.
const MAX_LINE: usize = 8 * 1024 * 1024;
/// Сколько stderr хранить для разбора ошибки.
const STDERR_KEEP: usize = 64 * 1024;
/// После выхода корня дочитываем хвост не дольше этого (внук мог унаследовать канал).
const DRAIN: Duration = Duration::from_millis(300);
/// Сколько ждём освобождения процесса после убийства.
const REAP: Duration = Duration::from_secs(2);

/// Сроки одного запуска.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Без единого байта stdout дольше этого процесс считается молчащим.
    pub idle: Duration,
    /// Потолок всего вызова.
    pub total: Duration,
    /// Сколько после `result` ждать самостоятельного выхода, прежде чем убить дерево.
    pub exit_grace: Duration,
}

/// Что делать после очередной строки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Терминальное событие получено: читать больше не нужно.
    Done,
    /// Получатель ушёл: убить сразу.
    Stop,
}

/// Чем кончился запуск.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Процесс вышел сам до терминального события.
    Exited(Option<i32>),
    /// Терминальное событие было; код выхода, если успел.
    Done(Option<i32>),
    Idle(Duration),
    Total(Duration),
    Stopped,
    Spawn(String),
}

#[derive(Debug)]
pub struct Ran {
    pub end: End,
    /// Хвост stderr (до 64 КиБ), целиком через `decode_process_output`.
    pub stderr: String,
    pub lines: usize,
}

impl Ran {
    fn spawn_failed(error: &std::io::Error) -> Self {
        Self {
            end: End::Spawn(error.to_string()),
            stderr: String::new(),
            lines: 0,
        }
    }
}

fn spawn(launch: &Launch) -> std::io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(&launch.program);
    command
        .args(&launch.args)
        .env_clear()
        .envs(launch.env.iter().map(|(key, value)| (key, value)))
        .current_dir(&launch.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Одного `kill_on_drop` мало: на Windows он бьёт только корень, внуки живут.
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    command.spawn()
}

async fn write_stdin(mut stdin: tokio::process::ChildStdin, bytes: Vec<u8>) {
    // Ребёнок мог закрыть вход раньше (упал при старте): ошибку записи разберёт выход.
    let _ = stdin.write_all(&bytes).await;
    let _ = stdin.shutdown().await;
}

type Shared = Arc<Mutex<Vec<u8>>>;

async fn keep_head(mut pipe: impl AsyncRead + Unpin, kept: Shared, cap: usize) {
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        if let Ok(mut kept) = kept.lock() {
            let room = cap.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
    }
}

fn shared_text(kept: &Shared) -> String {
    kept.lock()
        .map(|bytes| decode_process_output(&bytes).into_owned())
        .unwrap_or_default()
}

/// Режет поток на строки по `\n`; слишком длинную строку отбрасывает целиком.
struct Lines {
    pending: Vec<u8>,
    skipping: bool,
}

impl Lines {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            skipping: false,
        }
    }

    fn push(&mut self, chunk: &[u8], on_line: &mut impl FnMut(&[u8]) -> Flow) -> Flow {
        let mut rest = chunk;
        while let Some(at) = rest.iter().position(|byte| *byte == b'\n') {
            let (head, tail) = rest.split_at(at);
            rest = &tail[1..];
            if std::mem::take(&mut self.skipping) {
                self.pending.clear();
                continue;
            }
            self.pending.extend_from_slice(head);
            let flow = if self.pending.len() > MAX_LINE {
                Flow::Continue
            } else {
                on_line(&self.pending)
            };
            self.pending.clear();
            if flow != Flow::Continue {
                return flow;
            }
        }
        if !self.skipping {
            self.pending.extend_from_slice(rest);
            if self.pending.len() > MAX_LINE {
                self.pending.clear();
                self.skipping = true;
            }
        }
        Flow::Continue
    }

    fn finish(&mut self, on_line: &mut impl FnMut(&[u8]) -> Flow) -> Flow {
        if self.skipping || self.pending.is_empty() {
            return Flow::Continue;
        }
        let flow = on_line(&self.pending);
        self.pending.clear();
        flow
    }
}

fn trim_cr(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

async fn reap(child: &mut tokio::process::Child) -> Option<i32> {
    tokio::time::timeout(REAP, child.wait())
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|status| status.code())
}

/// Запустить и читать stdout построчно. Дерево процессов гаснет при любом выходе из функции.
pub async fn run_streaming(
    launch: &Launch,
    limits: &Limits,
    mut on_line: impl FnMut(&str) -> Flow,
) -> Ran {
    let mut child = match spawn(launch) {
        Ok(child) => child,
        Err(error) => return Ran::spawn_failed(&error),
    };
    // Объявлено после `child`: при отмене сначала падает дерево, потом корень.
    let tree = ProcessTree::adopt(&child);
    let _stdin = child
        .stdin
        .take()
        .map(|stdin| AbortOnDrop::spawn(write_stdin(stdin, launch.stdin.clone())));
    let stderr_kept: Shared = Arc::default();
    let _stderr = child
        .stderr
        .take()
        .map(|pipe| AbortOnDrop::spawn(keep_head(pipe, Arc::clone(&stderr_kept), STDERR_KEEP)));
    let Some(mut stdout) = child.stdout.take() else {
        drop(tree);
        return Ran::spawn_failed(&std::io::Error::other("stdout is not piped"));
    };

    let started = tokio::time::Instant::now();
    let mut lines = Lines::new();
    let mut count = 0usize;
    let mut handle = |bytes: &[u8]| {
        count += 1;
        on_line(&decode_process_output(trim_cr(bytes)))
    };
    let mut buf = vec![0u8; 16 * 1024];
    let mut exited: Option<Option<i32>> = None;
    let mut flow = Flow::Continue;
    let mut timed_out: Option<End> = None;

    loop {
        let remaining = limits.total.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            timed_out = Some(End::Total(limits.total));
            break;
        }
        let (wait, is_idle) = match exited {
            Some(_) => (DRAIN.min(remaining), false),
            None => (limits.idle.min(remaining), limits.idle <= remaining),
        };
        let read = tokio::select! {
            biased;
            status = child.wait(), if exited.is_none() => {
                exited = Some(status.ok().and_then(|s| s.code()));
                continue;
            }
            read = tokio::time::timeout(wait, stdout.read(&mut buf)) => read,
        };
        match read {
            Err(_) if exited.is_some() => break,
            Err(_) => {
                timed_out = Some(if is_idle {
                    End::Idle(limits.idle)
                } else {
                    End::Total(limits.total)
                });
                break;
            }
            Ok(Ok(0) | Err(_)) => {
                flow = lines.finish(&mut handle);
                break;
            }
            Ok(Ok(n)) => {
                flow = lines.push(&buf[..n], &mut handle);
                if flow != Flow::Continue {
                    break;
                }
            }
        }
    }

    let end = match (timed_out, flow) {
        (Some(end), _) => end,
        (None, Flow::Stop) => End::Stopped,
        (None, Flow::Done) => {
            let code = match exited {
                Some(code) => code,
                None => tokio::time::timeout(limits.exit_grace, child.wait())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|status| status.code()),
            };
            End::Done(code)
        }
        (None, Flow::Continue) => {
            let code = match exited {
                Some(code) => code,
                None => tokio::time::timeout(limits.exit_grace, child.wait())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|status| status.code()),
            };
            End::Exited(code)
        }
    };
    // Дерево гасим всегда: корень мог выйти, а внук остаться; затем освобождаем корень.
    drop(tree);
    let _ = reap(&mut child).await;
    let stderr = shared_text(&stderr_kept);
    Ran {
        end,
        stderr,
        lines: count,
    }
}

/// Итог короткой команды (`--version`, `models`, `auth status`).
#[derive(Debug, Default)]
pub struct Captured {
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Почему результата нет: не запустился или не уложился в срок.
    pub failure: Option<String>,
}

/// Запустить и дочитать вывод целиком; декодирование один раз по всему буферу.
pub async fn run_captured(launch: &Launch, timeout: Duration) -> Captured {
    let mut child = match spawn(launch) {
        Ok(child) => child,
        Err(error) => {
            return Captured {
                failure: Some(format!(
                    "cannot start {}: {error}",
                    launch.program.display()
                )),
                ..Captured::default()
            };
        }
    };
    let tree = ProcessTree::adopt(&child);
    let _stdin = child
        .stdin
        .take()
        .map(|stdin| AbortOnDrop::spawn(write_stdin(stdin, launch.stdin.clone())));
    let out: Shared = Arc::default();
    let err: Shared = Arc::default();
    let _out = child
        .stdout
        .take()
        .map(|pipe| AbortOnDrop::spawn(keep_head(pipe, Arc::clone(&out), 1024 * 1024)));
    let _err = child
        .stderr
        .take()
        .map(|pipe| AbortOnDrop::spawn(keep_head(pipe, Arc::clone(&err), STDERR_KEEP)));
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let (exit, failure) = match waited {
        Ok(Ok(status)) => (status.code(), None),
        Ok(Err(error)) => (None, Some(format!("wait failed: {error}"))),
        Err(_) => (
            None,
            Some(format!("no answer within {}s", timeout.as_secs())),
        ),
    };
    // Дочитать то, что успел отдать процесс, и убить дерево.
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(tree);
    let _ = reap(&mut child).await;
    Captured {
        exit,
        stdout: shared_text(&out),
        stderr: shared_text(&err),
        failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Lines::new();
        let mut seen = Vec::new();
        let mut handle = |bytes: &[u8]| {
            seen.push(String::from_utf8_lossy(bytes).into_owned());
            Flow::Continue
        };
        for chunk in chunks {
            lines.push(chunk, &mut handle);
        }
        lines.finish(&mut handle);
        seen
    }

    #[test]
    fn lines_survive_any_chunk_boundary_even_inside_a_multibyte_char() {
        let text = "{\"t\":\"привет 🦀\"}\n{\"t\":2}\n";
        let bytes = text.as_bytes();
        for split in 1..bytes.len() {
            let got = collect(&[&bytes[..split], &bytes[split..]]);
            assert_eq!(
                got,
                ["{\"t\":\"привет 🦀\"}", "{\"t\":2}"],
                "split at {split}"
            );
        }
    }

    #[test]
    fn a_last_line_without_a_newline_is_still_delivered() {
        assert_eq!(collect(&[b"a\nb"]), ["a", "b"]);
    }

    #[test]
    fn crlf_is_trimmed() {
        assert_eq!(trim_cr(b"x\r"), b"x");
        assert_eq!(trim_cr(b"x"), b"x");
    }

    #[test]
    fn an_oversized_line_is_dropped_and_the_next_one_still_arrives() {
        let mut lines = Lines::new();
        let mut seen = Vec::new();
        let mut handle = |bytes: &[u8]| {
            seen.push(bytes.len());
            Flow::Continue
        };
        let big = vec![b'x'; MAX_LINE / 2 + 1];
        lines.push(&big, &mut handle);
        lines.push(&big, &mut handle);
        lines.push(b"\nok\n", &mut handle);
        assert_eq!(seen, [2]);
    }

    #[test]
    fn a_stop_from_the_callback_ends_the_batch() {
        let mut lines = Lines::new();
        let mut calls = 0;
        let mut handle = |_: &[u8]| {
            calls += 1;
            Flow::Done
        };
        assert_eq!(lines.push(b"a\nb\n", &mut handle), Flow::Done);
        assert_eq!(calls, 1);
    }
}
