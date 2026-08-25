//! Pattern mining over accumulated traces and saved sessions.
//!
//! A single trace tells you what one turn did; a hundred of them tell you
//! what the agent *tends* to get wrong. The whole trick is normalisation:
//! raw messages are nearly unique (paths, counters, ids), so they are
//! stripped down to a shape before counting. Buckets ranked by frequency
//! are the work list.
//!
//! Sessions are mined too — the app has been saving them since long before
//! this harness existed, so there is a real corpus of live turns to learn
//! from (`--sessions`).

use crate::error::AppResult;
use crate::harness::MineArgs;
use crate::harness::metrics::{RunMetrics, message_field};
use crate::harness::trace::Trace;
use crate::session::Session;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Below this length a repeated assistant line carries no signal — code
/// fences and one-word acknowledgements repeat in healthy sessions too.
const MIN_LOOP_SHAPE_CHARS: usize = 16;

/// One counted pattern.
#[derive(Debug, Clone, Serialize)]
pub struct Pattern {
    pub shape: String,
    pub count: usize,
    /// A verbatim instance, so the shape can be traced back to reality.
    pub example: String,
}

/// A named group of patterns.
#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    pub name: &'static str,
    pub what_it_means: &'static str,
    pub total: usize,
    pub patterns: Vec<Pattern>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MineReport {
    pub traces_scanned: usize,
    pub sessions_scanned: usize,
    pub damaged_traces: usize,
    /// Turn endings across every trace — the headline health number.
    pub turn_ends: BTreeMap<String, usize>,
    pub buckets: Vec<Bucket>,
}

/// Accumulates counts before they are ranked.
#[derive(Default)]
struct Counter {
    counts: BTreeMap<String, (usize, String)>,
}

impl Counter {
    fn add(&mut self, raw: &str) {
        let shape = normalize(raw);
        let entry = self
            .counts
            .entry(shape)
            .or_insert_with(|| (0, crate::util::truncate_with_ellipsis(raw, 300)));
        entry.0 += 1;
    }

    fn total(&self) -> usize {
        self.counts.values().map(|(count, _)| count).sum()
    }

    fn top(&self, limit: usize) -> Vec<Pattern> {
        let mut patterns: Vec<Pattern> = self
            .counts
            .iter()
            .map(|(shape, (count, example))| Pattern {
                shape: shape.clone(),
                count: *count,
                example: example.clone(),
            })
            .collect();
        // Count first, then shape, so equal counts render in a stable order.
        patterns.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.shape.cmp(&b.shape)));
        patterns.truncate(limit);
        patterns
    }
}

pub fn run(args: MineArgs) -> AppResult<i32> {
    let paths = if args.paths.is_empty() {
        vec![PathBuf::from(super::DEFAULT_OUT_DIR)]
    } else {
        args.paths.clone()
    };

    let mut report = MineReport::default();
    let mut malformed = Counter::default();
    let mut tool_errors = Counter::default();
    let mut stream_problems = Counter::default();
    let mut hint_misses = Counter::default();
    let mut repeated_answers = Counter::default();

    for file in collect_traces(&paths) {
        let Ok(trace) = Trace::read(&file) else {
            continue;
        };
        report.traces_scanned += 1;
        if trace.unparsable_lines > 0 {
            report.damaged_traces += 1;
        }
        let metrics = RunMetrics::from_trace(&trace);
        *report
            .turn_ends
            .entry(format!("{:?}", metrics.turn_end).to_lowercase())
            .or_default() += 1;

        for record in trace.by_action("agent.step.malformed_tool_use") {
            if let Some(errors) = message_field(record, "errors") {
                malformed.add(&errors);
            }
        }
        for record in trace.by_action("agent.tool.result.payload") {
            let data = record.data.as_ref();
            let is_error = data
                .and_then(|data| data.get("is_error"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !is_error {
                continue;
            }
            let text = data
                .and_then(|data| data.get("result"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let name = record.field("tool_name").unwrap_or("?");
            tool_errors.add(&format!("{name}: {}", first_line(text)));
        }
        for record in trace.by_action("agent.step.error") {
            if let Some(reason) = message_field(record, "reason") {
                stream_problems.add(&reason);
            }
        }
        for _ in trace.by_action("agent.step.stream_closed") {
            stream_problems.add("stream_closed");
        }

        // A hint that fired but whose suggestions were never used is the
        // signal that retrieval is pointing at the wrong thing.
        if let Some(hint) = &metrics.semantic_hint
            && metrics.tool_calls == 0
        {
            hint_misses.add(hint);
        }
    }

    if args.sessions {
        for session in load_sessions() {
            report.sessions_scanned += 1;
            let mut seen: BTreeMap<String, usize> = BTreeMap::new();
            for message in &session.messages {
                if message.role != crate::provider::Role::Assistant {
                    continue;
                }
                let key = normalize(first_line(&message.content));
                *seen.entry(key).or_default() += 1;
            }
            // The same assistant line twice in one session is the shape of a
            // loop the model could not break out of. Short shapes are
            // dropped: a repeated "```xml" or "ok" is punctuation, not a loop.
            for (shape, count) in seen {
                if count > 1 && shape.chars().count() >= MIN_LOOP_SHAPE_CHARS {
                    repeated_answers.add(&shape);
                }
            }
        }
    }

    report.buckets = vec![
        bucket(
            "malformed-tool-calls",
            "parser feedback the model triggered — prompt or grammar problems",
            &malformed,
            args.top,
        ),
        bucket(
            "tool-errors",
            "tools that ran and failed — environment or argument problems",
            &tool_errors,
            args.top,
        ),
        bucket(
            "stream-problems",
            "provider-side stream failures",
            &stream_problems,
            args.top,
        ),
        bucket(
            "hints-without-tool-use",
            "retrieval fired but the turn used no tools — possible bad hints",
            &hint_misses,
            args.top,
        ),
        bucket(
            "repeated-answers",
            "the same assistant line more than once in a session — loops",
            &repeated_answers,
            args.top,
        ),
    ];

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", render(&report));
    }
    Ok(0)
}

fn bucket(
    name: &'static str,
    what_it_means: &'static str,
    counter: &Counter,
    top: usize,
) -> Bucket {
    Bucket {
        name,
        what_it_means,
        total: counter.total(),
        patterns: counter.top(top),
    }
}

/// Strip the parts that make otherwise-identical messages unique, so the
/// same failure seen on two machines lands in one bucket.
///
/// Three passes, in this order: quoted spans (which may contain both spaces
/// and separators) collapse first, then whole path-like tokens, then digit
/// runs. Collapsing paths before quotes would shred a quoted path into
/// fragments and split the bucket again.
fn normalize(text: &str) -> String {
    let text = crate::util::truncate_at_char_boundary(text.trim(), 400);
    collapse_quoted(text)
        .split_whitespace()
        .map(normalize_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Replace every `"…"` / `'…'` span with `<str>`. An unterminated quote
/// swallows the rest of the line on purpose: a message truncated mid-quote
/// should not fork off into a bucket of its own.
fn collapse_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            out.push_str("<str>");
            for inner in chars.by_ref() {
                if inner == c {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// One whitespace-separated token, reduced to its shape.
fn normalize_token(token: &str) -> String {
    // A token carrying a path separator is a path: its segments are unique
    // per machine and per run, so only the fact that it was a path matters.
    if token.contains('/') || token.contains('\\') {
        return "<path>".to_string();
    }
    let mut out = String::with_capacity(token.len());
    let mut in_number = false;
    for c in token.chars() {
        if c.is_ascii_digit() {
            if !in_number {
                out.push_str("<n>");
                in_number = true;
            }
        } else {
            in_number = false;
            out.push(c);
        }
    }
    out
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// Every `.jsonl` under the given files/directories.
fn collect_traces(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(current) = stack.pop() {
        if current.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&current) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if current.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(current);
        }
    }
    files.sort();
    files
}

fn load_sessions() -> Vec<Session> {
    let dir = crate::config::Config::sessions_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| read_session(&path))
        .collect()
}

fn read_session(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn render(report: &MineReport) -> String {
    let mut lines = vec![format!(
        "{} trace(s), {} session(s) scanned{}",
        report.traces_scanned,
        report.sessions_scanned,
        if report.damaged_traces > 0 {
            format!(" — {} damaged", report.damaged_traces)
        } else {
            String::new()
        }
    )];
    if !report.turn_ends.is_empty() {
        let ends: Vec<String> = report
            .turn_ends
            .iter()
            .map(|(end, count)| format!("{end}×{count}"))
            .collect();
        lines.push(format!("turn endings: {}", ends.join(", ")));
    }
    for bucket in &report.buckets {
        lines.push(String::new());
        lines.push(format!(
            "── {} ({}) — {}",
            bucket.name, bucket.total, bucket.what_it_means
        ));
        if bucket.patterns.is_empty() {
            lines.push("   (none)".to_string());
            continue;
        }
        for pattern in &bucket.patterns {
            lines.push(format!("   {:>4}× {}", pattern.count, pattern.shape));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_collapses_ids_paths_and_numbers() {
        let windows = normalize("failed to read E:\\Projects\\a\\b.rs at line 42");
        let unix = normalize("failed to read /home/u/c/d.rs at line 7");
        assert_eq!(
            windows, unix,
            "different paths and line numbers must share a shape"
        );
        assert_eq!(windows, "failed to read <path> at line <n>");
    }

    #[test]
    fn a_bare_number_keeps_its_shape() {
        assert_eq!(normalize("exit code 137"), "exit code <n>");
        // A step counter reads as a path, which is fine: both halves are
        // volatile and the bucket only needs the surrounding words.
        assert_eq!(normalize("step 1/6"), "step <path>");
    }

    #[test]
    fn normalization_collapses_quoted_spans_even_with_spaces() {
        assert_eq!(
            normalize("unknown tool \"frobnicate\""),
            normalize("unknown tool \"widget\"")
        );
        // A quoted path holds both spaces and separators, so quotes have to
        // collapse before tokenizing or this splits into three buckets.
        assert_eq!(
            normalize("cannot open \"C:\\Program Files\\x.exe\" here"),
            "cannot open <str> here"
        );
    }

    #[test]
    fn normalization_squeezes_whitespace() {
        assert_eq!(normalize("  a \n\t b  "), "a b");
    }

    #[test]
    fn counter_ranks_by_frequency_and_keeps_an_example() {
        let mut counter = Counter::default();
        counter.add("bad json at 1");
        counter.add("bad json at 22");
        counter.add("missing tag");
        assert_eq!(counter.total(), 3);
        let top = counter.top(10);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].count, 2);
        assert!(top[0].example.starts_with("bad json at"));
    }

    #[test]
    fn counter_top_respects_the_limit() {
        let mut counter = Counter::default();
        for i in 0..5 {
            counter.add(&format!("distinct shape {}", "x".repeat(i + 1)));
        }
        assert_eq!(counter.top(2).len(), 2);
    }

    #[test]
    fn loop_threshold_excludes_punctuation_shapes() {
        assert!("```xml".chars().count() < MIN_LOOP_SHAPE_CHARS);
        assert!(
            "I could not find that file".chars().count() >= MIN_LOOP_SHAPE_CHARS,
            "a real repeated sentence must survive the filter"
        );
    }

    #[test]
    fn first_line_is_trimmed_and_total() {
        assert_eq!(first_line("  one  \ntwo"), "one");
        assert_eq!(first_line(""), "");
    }
}
