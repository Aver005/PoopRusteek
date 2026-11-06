//! Учёт токенов по аккаунту и модели. Записи копятся в памяти (быстро, без ввода-вывода),
//! агрегаты по дням уходят на диск через `util::atomic_write` фоновой задачей раз в ~30 с.
//!
//! Сырые вызовы на диск не пишутся. Одна и та же запись (`request_id` + `attempt` +
//! аккаунт) считается один раз: повторная отправка той же записи ничего не добавляет.

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

#[derive(Default)]
struct Inner {
    rows: BTreeMap<Key, Agg>,
    dirty: bool,
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
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let file: LedgerFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!("gateway usage file ignored: {error}");
                return;
            }
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for row in file.rows {
            inner.rows.insert(
                Key {
                    day: row.day,
                    account: row.account,
                    model: row.model,
                    source: row.source,
                },
                row.agg,
            );
        }
    }

    /// Записать на диск, если что-то изменилось. `Ok(false)` — писать было нечего.
    pub fn flush(&self) -> std::io::Result<bool> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let bytes = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| std::io::Error::other("lock"))?;
            if !inner.dirty {
                return Ok(false);
            }
            inner.dirty = false;
            let file = LedgerFile {
                version: 1,
                rows: inner
                    .rows
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
            serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?
        };
        let written = crate::util::atomic_write(path, &bytes);
        if written.is_err()
            && let Ok(mut inner) = self.inner.lock()
        {
            inner.dirty = true;
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
        let agg = inner.rows.entry(key).or_default();
        agg.calls += 1;
        if matches!(record.outcome, Outcome::Failed(_)) {
            agg.failed += 1;
        }
        if record.fallback_from.is_some() {
            agg.fallbacks += 1;
        }
        if let Some(usage) = &record.usage {
            agg.prompt_tokens += u64::from(usage.prompt_tokens);
            agg.completion_tokens += u64::from(usage.completion_tokens);
            agg.cache_read_tokens += u64::from(usage.cache_read_tokens.unwrap_or(0));
            agg.estimated |= record.estimated;
        }
        inner.dirty = true;
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
