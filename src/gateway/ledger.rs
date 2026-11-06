//! Учёт токенов по аккаунту и модели: копится в памяти, раз в ~30 с сливается с файлом через `atomic_write`.
//! Повтор той же записи (`request_id` + `attempt` + аккаунт) не считается дважды.

use super::clock::to_unix;
use super::handle::{CallSource, Ledger, LedgerQuery, LedgerRow, Outcome, UsageRecord};
use super::types::{AccountId, Role};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Сколько последних записей помнить для защиты от двойного учёта.
const SEEN_LIMIT: usize = 4096;
const DAY_SECS: u64 = 86_400;

fn source_key(source: CallSource) -> String {
    match source {
        CallSource::Chat => "chat".into(),
        CallSource::SubAgent => "sub_agent".into(),
        CallSource::Serve => "serve".into(),
        CallSource::Role(role) => format!("role:{}", role_key(role)),
    }
}

fn role_key(role: Role) -> &'static str {
    match role {
        Role::Main => "main",
        Role::Title => "title",
        Role::Summary => "summary",
        Role::Evaluator => "evaluator",
    }
}

fn parse_source(raw: &str) -> Option<CallSource> {
    Some(match raw {
        "chat" => CallSource::Chat,
        "sub_agent" => CallSource::SubAgent,
        "serve" => CallSource::Serve,
        other => CallSource::Role(match other.strip_prefix("role:")? {
            "main" => Role::Main,
            "title" => Role::Title,
            "summary" => Role::Summary,
            "evaluator" => Role::Evaluator,
            _ => return None,
        }),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    day: u64,
    account: String,
    model: String,
    source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Agg {
    calls: u64,
    failed: u64,
    fallbacks: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    estimated: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Row {
    day: u64,
    account: String,
    model: String,
    source: String,
    #[serde(flatten)]
    agg: Agg,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    rows: Vec<Row>,
}

impl Agg {
    fn add(&mut self, other: &Agg) {
        self.calls += other.calls;
        self.failed += other.failed;
        self.fallbacks += other.fallbacks;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.estimated |= other.estimated;
    }
}

/// Строки файла учёта. Нет файла или он битый — пусто, и это не ошибка.
fn read_rows(path: &std::path::Path) -> BTreeMap<Key, Agg> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let file: LedgerFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!("gateway usage file ignored: {error}");
            return BTreeMap::new();
        }
    };
    file.rows
        .into_iter()
        .map(|row| {
            let key = Key {
                day: row.day,
                account: row.account,
                model: row.model,
                source: row.source,
            };
            (key, row.agg)
        })
        .collect()
}

#[derive(Default)]
struct Inner {
    /// Всё известное: прочитанное с диска плюс то, что набрано с последнего сброса.
    rows: BTreeMap<Key, Agg>,
    /// Только набранное с последнего сброса: сливается с диском, а не заменяет его.
    pending: BTreeMap<Key, Agg>,
    seen: HashSet<(String, u8, String)>,
    order: VecDeque<(String, u8, String)>,
}

impl Inner {
    /// `true`, если запись новая; без `request_id` отличить повтор нечем.
    fn first_time(&mut self, record: &UsageRecord) -> bool {
        let Some(request_id) = &record.request_id else {
            return true;
        };
        let key = (
            request_id.clone(),
            record.attempt,
            record.account.to_string(),
        );
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > SEEN_LIMIT
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        true
    }
}

pub struct EngineLedger {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl EngineLedger {
    /// Без пути учёт живёт только в памяти (тесты, `DisabledGateway`-подобные сборки).
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        let ledger = Self {
            inner: Mutex::new(Inner::default()),
            path,
        };
        ledger.load();
        Arc::new(ledger)
    }

    /// Битый файл не мешает работе: учёт начнётся с нуля и перепишет его.
    fn load(&self) {
        let Some(path) = &self.path else { return };
        let rows = read_rows(path);
        if let Ok(mut inner) = self.inner.lock() {
            inner.rows = rows;
        }
    }

    /// Слить набранное с файлом на диске и записать: чужие процессы свои строки не теряют.
    /// Между чтением и записью остаётся маленькая гонка, потерять всё накопленное она не может.
    pub fn flush(&self) -> std::io::Result<bool> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let pending = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| std::io::Error::other("lock"))?;
            if inner.pending.is_empty() {
                return Ok(false);
            }
            std::mem::take(&mut inner.pending)
        };
        let mut merged = read_rows(path);
        for (key, agg) in &pending {
            merged.entry(key.clone()).or_default().add(agg);
        }
        let file = LedgerFile {
            version: 1,
            rows: merged
                .iter()
                .map(|(key, agg)| Row {
                    day: key.day,
                    account: key.account.clone(),
                    model: key.model.clone(),
                    source: key.source.clone(),
                    agg: agg.clone(),
                })
                .collect(),
        };
        let written = serde_json::to_vec_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|bytes| crate::util::atomic_write(path, &bytes));
        let Ok(mut inner) = self.inner.lock() else {
            return written.map(|()| true);
        };
        match &written {
            Ok(()) => {
                // Вид теперь включает чужое с диска и то, что набралось за время записи.
                let since = std::mem::take(&mut inner.pending);
                inner.rows = merged;
                for (key, agg) in &since {
                    inner.rows.entry(key.clone()).or_default().add(agg);
                }
                inner.pending = since;
            }
            Err(_) => {
                for (key, agg) in pending {
                    inner.pending.entry(key).or_default().add(&agg);
                }
            }
        }
        written.map(|()| true)
    }
}

impl Drop for EngineLedger {
    /// Последний сброс при выходе; потеря при падении — не больше интервала.
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl Ledger for EngineLedger {
    fn record(&self, record: UsageRecord) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.first_time(&record) {
            return;
        }
        let key = Key {
            day: to_unix(record.at) / DAY_SECS,
            account: record.account.to_string(),
            model: record.model.clone(),
            source: source_key(record.source),
        };
        let mut agg = Agg {
            calls: 1,
            failed: u64::from(matches!(record.outcome, Outcome::Failed(_))),
            fallbacks: u64::from(record.fallback_from.is_some()),
            ..Agg::default()
        };
        if let Some(usage) = &record.usage {
            agg.prompt_tokens = u64::from(usage.prompt_tokens);
            agg.completion_tokens = u64::from(usage.completion_tokens);
            agg.cache_read_tokens = u64::from(usage.cache_read_tokens.unwrap_or(0));
            agg.estimated = record.estimated;
        }
        inner.rows.entry(key.clone()).or_default().add(&agg);
        inner.pending.entry(key).or_default().add(&agg);
    }

    /// Суммы за все дни не раньше дня `since`; день целиком (границу не режем).
    fn totals(&self, query: &LedgerQuery) -> Vec<LedgerRow> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let since_day = query.since.map(|t| to_unix(t) / DAY_SECS);
        let mut sums: BTreeMap<(String, String, String), LedgerRow> = BTreeMap::new();
        for (key, agg) in &inner.rows {
            if since_day.is_some_and(|day| key.day < day) {
                continue;
            }
            if query
                .account
                .as_ref()
                .is_some_and(|wanted| wanted.as_str() != key.account)
            {
                continue;
            }
            let (Ok(account), Some(source)) =
                (AccountId::parse(&key.account), parse_source(&key.source))
            else {
                continue;
            };
            let row = sums
                .entry((key.account.clone(), key.model.clone(), key.source.clone()))
                .or_insert_with(|| LedgerRow {
                    account,
                    model: key.model.clone(),
                    source,
                    calls: 0,
                    failed: 0,
                    fallbacks: 0,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cache_read_tokens: 0,
                    any_estimated: false,
                });
            row.calls += agg.calls;
            row.failed += agg.failed;
            row.fallbacks += agg.fallbacks;
            row.prompt_tokens += agg.prompt_tokens;
            row.completion_tokens += agg.completion_tokens;
            row.cache_read_tokens += agg.cache_read_tokens;
            row.any_estimated |= agg.estimated;
        }
        sums.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::clock::{from_unix, scratch_dir};
    use super::*;
    use crate::provider::Usage;

    fn record(
        account: &str,
        request: Option<&str>,
        attempt: u8,
        tokens: (u32, u32),
    ) -> UsageRecord {
        UsageRecord {
            at: from_unix(10 * DAY_SECS + 5),
            source: CallSource::Chat,
            account: AccountId::parse(account).unwrap(),
            model: "m".into(),
            route: None,
            usage: Some(Usage {
                prompt_tokens: tokens.0,
                completion_tokens: tokens.1,
                total_tokens: tokens.0 + tokens.1,
                cache_read_tokens: Some(tokens.0 / 2),
                ..Usage::default()
            }),
            estimated: false,
            cost_usd: None,
            latency_ms: 10,
            ttft_ms: None,
            outcome: Outcome::Ok,
            fallback_from: None,
            attempt,
            request_id: request.map(str::to_string),
        }
    }

    #[test]
    fn tokens_add_up_per_account_and_model() {
        let ledger = EngineLedger::new(None);
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        ledger.record(record("a", Some("r2"), 1, (200, 20)));
        ledger.record(record("b", Some("r3"), 1, (5, 1)));
        let rows = ledger.totals(&LedgerQuery::default());
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r.account.as_str() == "a").unwrap();
        assert_eq!(
            (a.calls, a.prompt_tokens, a.completion_tokens),
            (2, 300, 30)
        );
        assert_eq!(a.cache_read_tokens, 150);
        let only_b = ledger.totals(&LedgerQuery {
            account: Some(AccountId::parse("b").unwrap()),
            since: None,
        });
        assert_eq!(only_b.len(), 1, "the account filter works");
    }

    /// Повтор той же записи не считается дважды; новая попытка — отдельная запись.
    #[test]
    fn the_same_record_is_counted_once_but_a_new_attempt_is_a_new_record() {
        let ledger = EngineLedger::new(None);
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        let once = &ledger.totals(&LedgerQuery::default())[0];
        assert_eq!((once.calls, once.prompt_tokens), (1, 100));
        ledger.record(record("a", Some("r1"), 2, (100, 10)));
        let twice = &ledger.totals(&LedgerQuery::default())[0];
        assert_eq!(
            (twice.calls, twice.prompt_tokens),
            (2, 200),
            "a retry really cost tokens"
        );
        // Без номера запроса отличить повтор не от чего: каждая запись своя.
        ledger.record(record("a", None, 1, (1, 1)));
        ledger.record(record("a", None, 1, (1, 1)));
        assert_eq!(ledger.totals(&LedgerQuery::default())[0].calls, 4);
    }

    #[test]
    fn failures_and_fallbacks_are_counted_but_cost_no_tokens() {
        let ledger = EngineLedger::new(None);
        let mut failed = record("a", Some("r"), 1, (0, 0));
        failed.usage = None;
        failed.outcome = Outcome::Failed("rate_limit".into());
        ledger.record(failed);
        let mut after = record("b", Some("r"), 2, (50, 5));
        after.fallback_from = Some(AccountId::parse("a").unwrap());
        ledger.record(after);
        let rows = ledger.totals(&LedgerQuery::default());
        let a = rows.iter().find(|r| r.account.as_str() == "a").unwrap();
        let b = rows.iter().find(|r| r.account.as_str() == "b").unwrap();
        assert_eq!((a.calls, a.failed, a.prompt_tokens), (1, 1, 0));
        assert_eq!((b.fallbacks, b.prompt_tokens), (1, 50));
    }

    #[test]
    fn estimates_are_marked_and_the_since_filter_drops_old_days() {
        let ledger = EngineLedger::new(None);
        let mut estimated = record("a", Some("e"), 1, (10, 1));
        estimated.estimated = true;
        ledger.record(estimated);
        let mut old = record("a", Some("o"), 1, (1000, 1));
        old.at = from_unix(2 * DAY_SECS);
        ledger.record(old);
        let recent = ledger.totals(&LedgerQuery {
            account: None,
            since: Some(from_unix(9 * DAY_SECS)),
        });
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].prompt_tokens, 10, "the old day is out");
        assert!(recent[0].any_estimated);
    }

    #[test]
    fn totals_survive_a_flush_and_a_reload() {
        let path = scratch_dir("ledger").join("gateway").join("usage.json");
        let ledger = EngineLedger::new(Some(path.clone()));
        assert!(!ledger.flush().unwrap(), "nothing to write yet");
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        assert!(ledger.flush().unwrap());
        assert!(!ledger.flush().unwrap(), "clean after a flush");
        drop(ledger);
        let reloaded = EngineLedger::new(Some(path));
        let rows = reloaded.totals(&LedgerQuery::default());
        assert_eq!((rows[0].calls, rows[0].prompt_tokens), (1, 100));
        assert_eq!(rows[0].source, CallSource::Chat);
    }

    /// Два процесса с одним `data_dir`: каждый сохраняет свои строки, итог — сумма.
    #[test]
    fn two_processes_flushing_the_same_file_add_up_instead_of_overwriting() {
        let path = scratch_dir("ledger-two").join("usage.json");
        let first = EngineLedger::new(Some(path.clone()));
        let second = EngineLedger::new(Some(path.clone()));
        first.record(record("a", Some("r1"), 1, (100, 10)));
        second.record(record("a", Some("r2"), 1, (30, 3)));
        second.record(record("b", Some("r3"), 1, (7, 1)));
        assert!(first.flush().unwrap());
        assert!(second.flush().unwrap());
        let on_disk = EngineLedger::new(Some(path));
        let rows = on_disk.totals(&LedgerQuery::default());
        let a = rows.iter().find(|r| r.account.as_str() == "a").unwrap();
        assert_eq!((a.calls, a.prompt_tokens), (2, 130), "{rows:?}");
        assert!(rows.iter().any(|r| r.account.as_str() == "b"));
        let seen_by_second: u64 = second
            .totals(&LedgerQuery::default())
            .iter()
            .map(|r| r.calls)
            .sum();
        assert_eq!(
            seen_by_second, 3,
            "the second process now sees the first call too"
        );
    }

    #[test]
    fn a_second_flush_adds_only_what_is_new() {
        let path = scratch_dir("ledger-twice").join("usage.json");
        let ledger = EngineLedger::new(Some(path.clone()));
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        ledger.flush().unwrap();
        ledger.record(record("a", Some("r2"), 1, (50, 5)));
        ledger.flush().unwrap();
        assert!(!ledger.flush().unwrap(), "nothing new, nothing written");
        let rows = EngineLedger::new(Some(path)).totals(&LedgerQuery::default());
        assert_eq!(
            (rows[0].calls, rows[0].prompt_tokens),
            (2, 150),
            "no double counting"
        );
    }

    #[test]
    fn a_failed_write_keeps_the_pending_counts_for_the_next_try() {
        // Путь под обычным файлом: записать нельзя.
        let dir = scratch_dir("ledger-bad-path");
        let blocker = dir.join("file");
        std::fs::write(&blocker, "x").unwrap();
        let ledger = EngineLedger::new(Some(blocker.join("usage.json")));
        ledger.record(record("a", Some("r1"), 1, (100, 10)));
        assert!(ledger.flush().is_err());
        assert_eq!(ledger.totals(&LedgerQuery::default())[0].calls, 1);
        std::fs::remove_file(&blocker).unwrap();
        assert!(
            ledger.flush().unwrap(),
            "the counts were kept and now go out"
        );
    }

    #[test]
    fn every_source_survives_the_key_round_trip() {
        for source in [
            CallSource::Chat,
            CallSource::SubAgent,
            CallSource::Serve,
            CallSource::Role(Role::Title),
            CallSource::Role(Role::Summary),
            CallSource::Role(Role::Evaluator),
            CallSource::Role(Role::Main),
        ] {
            assert_eq!(parse_source(&source_key(source)), Some(source));
        }
        assert_eq!(parse_source("nonsense"), None);
    }

    #[test]
    fn a_corrupt_usage_file_starts_a_fresh_count() {
        let dir = scratch_dir("ledger-bad");
        let path = dir.join("usage.json");
        std::fs::write(&path, "][").unwrap();
        let ledger = EngineLedger::new(Some(path));
        assert!(ledger.totals(&LedgerQuery::default()).is_empty());
    }
}
