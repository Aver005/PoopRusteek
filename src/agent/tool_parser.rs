use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static XML_TOOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<tool_use>\s*(.*?)\s*</tool_use>").expect("hardcoded regex is valid")
});
static XML_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<name>\s*(.*?)\s*</name>").expect("hardcoded regex is valid")
});
static XML_ARGS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<arguments>\s*(.*?)\s*</arguments>").expect("hardcoded regex is valid")
});
static LEGACY_TOOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[TOOL:([^\]]+)\]\s*(\{[^}]*\})").expect("hardcoded regex is valid")
});
static STRIP_TOOL_XML_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<tool_use>\s*.*?\s*</tool_use>").expect("hardcoded regex is valid")
});
static STRIP_THINKING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<thinking>\s*.*?\s*</thinking>").expect("hardcoded regex is valid")
});
static STRIP_LEGACY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[TOOL:[^\]]+\]\s*\{[^}]*\}").expect("hardcoded regex is valid"));

#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// Идентификатор от провайдера, если вызов пришёл родным протоколом.
    /// `None` — промптовый путь, там идентификатор придумывает сам цикл.
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

impl From<&crate::provider::ToolCall> for ParsedToolCall {
    fn from(call: &crate::provider::ToolCall) -> Self {
        Self {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        }
    }
}

/// Test-only convenience wrapper over [`parse_tool_calls_with_errors`] that
/// discards the diagnostics. Every production caller (main and sub-agent
/// loops) uses the `_with_errors` variant, so a malformed `<tool_use>` block
/// is handed back for a retry instead of silently ending the turn.
#[cfg(test)]
pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    parse_tool_calls_with_errors(text).0
}

/// Parse every tool call in `text`, returning both the successful calls and a
/// human-readable diagnostic for each `<tool_use>`/`[TOOL:…]` block that looked
/// like a call but failed to parse.
///
/// A non-empty error list with zero calls is the signature of the "frozen
/// agent" bug: a weaker model emits a `<tool_use>` block with swapped closing
/// tags or invalid JSON, every call silently drops, `strip_tool_calls` erases
/// the block, and the turn ends with nothing shown and nothing run. The runner
/// feeds these diagnostics back so the model can re-issue the call.
///
/// Tolerances beyond the strict `<name>`+`<arguments>` shape:
/// - swapped `</arguments>`/`</tool_use>` closing tags (take text after the
///   `<arguments>` opener up to whichever closer appears first);
/// - a dropped `<arguments>` wrapper (fall back to the first `{…}` after
///   `<name>`);
/// - the bare-JSON shapes `{"tool":…,"args":…}` and `{"name":…,"arguments":…}`.
///
/// Genuinely broken JSON (e.g. unescaped quotes around a Windows path) is *not*
/// guessed at — repairing it risks running the wrong command — it becomes a
/// diagnostic instead.
pub fn parse_tool_calls_with_errors(text: &str) -> (Vec<ParsedToolCall>, Vec<String>) {
    let mut calls = Vec::new();
    let mut errors = Vec::new();

    for cap in XML_TOOL_RE.captures_iter(text) {
        let body = cap[1].trim();

        if let Some(name_cap) = XML_NAME_RE.captures(body) {
            let name = name_cap[1].trim().to_string();
            match extract_arguments(body) {
                Some(args_str) => match serde_json::from_str::<Value>(args_str.trim()) {
                    Ok(arguments) => calls.push(ParsedToolCall {
                        id: None,
                        name,
                        arguments,
                    }),
                    Err(error) => errors.push(format!(
                        "tool `{name}`: <arguments> is not valid JSON ({error}). Re-send \
                         with a valid JSON object and escape every backslash (\\\\) and \
                         quote (\\\") inside string values."
                    )),
                },
                None => errors.push(format!(
                    "tool `{name}`: missing <arguments> block. Send exactly \
                     <tool_use><name>{name}</name><arguments>{{ ... }}</arguments></tool_use>."
                )),
            }
            continue;
        }

        // No <name> tag — accept the bare-JSON object shapes.
        match serde_json::from_str::<Value>(body) {
            Ok(value) => {
                let name = value
                    .get("tool")
                    .or_else(|| value.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let arguments = value
                    .get("args")
                    .or_else(|| value.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                match name {
                    Some(name) => calls.push(ParsedToolCall {
                        id: None,
                        name,
                        arguments,
                    }),
                    None => errors.push(
                        "a <tool_use> block had no <name> tag and no \"tool\"/\"name\" \
                         field in its JSON."
                            .to_string(),
                    ),
                }
            }
            Err(error) => errors.push(format!(
                "malformed <tool_use> block ({error}). Use \
                 <tool_use><name>TOOL</name><arguments>{{ valid JSON }}</arguments></tool_use>."
            )),
        }
    }

    for cap in LEGACY_TOOL_RE.captures_iter(text) {
        let name = cap[1].to_string();
        let args_str = &cap[2];

        match serde_json::from_str::<Value>(args_str) {
            Ok(args) => {
                calls.push(ParsedToolCall {
                    id: None,
                    name,
                    arguments: args,
                });
            }
            Err(e) => {
                errors.push(format!(
                    "tool `{name}`: arguments are not valid JSON ({e})."
                ));
            }
        }
    }

    (calls, errors)
}

/// Pull the JSON arguments out of a `<tool_use>` body, tolerating the common
/// malformations that weaker models produce. Returns the raw argument slice
/// (still to be JSON-parsed by the caller), or `None` when no arguments region
/// can be located at all.
fn extract_arguments(body: &str) -> Option<String> {
    // Well-formed <arguments>…</arguments>.
    if let Some(cap) = XML_ARGS_RE.captures(body) {
        return Some(cap[1].trim().to_string());
    }
    // Missing/misplaced </arguments> — most often swapped with </tool_use>
    // (`…</tool_use></arguments>`). The outer XML_TOOL_RE already cut at the
    // first </tool_use>, so `body` ends right after the JSON: take everything
    // past the opener, defensively trimming any stray closer.
    if let Some(idx) = body.find("<arguments>") {
        let mut rest = &body[idx + "<arguments>".len()..];
        if let Some(end) = rest.find("</arguments>") {
            rest = &rest[..end];
        }
        if let Some(end) = rest.find("</tool_use>") {
            rest = &rest[..end];
        }
        return Some(rest.trim().to_string());
    }
    // No <arguments> wrapper at all — fall back to the first JSON object after
    // <name>.
    let after_name = XML_NAME_RE
        .find(body)
        .map(|m| &body[m.end()..])
        .unwrap_or(body);
    let start = after_name.find('{')?;
    Some(after_name[start..].trim().to_string())
}

/// Убрать только рассуждения. На родном протоколе вызовов в тексте нет, и
/// вырезание `<tool_use>`/`[TOOL:…]` там — чистая потеря: модель, которую
/// спросили «как выглядит вызов в этом репозитории», лишилась бы ответа.
pub fn strip_thinking_only(text: &str) -> String {
    STRIP_THINKING_RE.replace_all(text, "").trim().to_string()
}

pub fn strip_tool_calls(text: &str) -> String {
    let without_xml = STRIP_TOOL_XML_RE.replace_all(text, "");
    let without_thinking = STRIP_THINKING_RE.replace_all(&without_xml, "");
    STRIP_LEGACY_RE
        .replace_all(without_thinking.trim(), "")
        .trim()
        .to_string()
}

/// Incremental equivalent of [`stream_visible_text`] for streaming.
///
/// The runner calls it once per SSE delta with the *whole* accumulated
/// response; re-running three regex `replace_all` passes over an
/// ever-growing string made per-turn streaming cost O(n²) in regex work.
/// The tracker freezes the input prefix whose *stripped* form provably
/// can't change anymore and re-runs the strip passes only on the remaining
/// "hot" tail — for plain prose that tail is empty. The cheap final
/// truncation step still runs over the whole stripped string every call,
/// deliberately: a `[TOOL:` cut marker can be spliced together across the
/// frozen/hot seam by a stripped block, so truncation must stay global to
/// match the non-incremental pipeline exactly.
///
/// Output is byte-identical to `stream_visible_text(full)` for every
/// prefix; `tracker_matches_full_pipeline_on_every_prefix` feeds both
/// paths chunk-by-chunk over adversarial cases (seam-spanning blocks,
/// reconstituted openers and markers) to enforce that.
#[derive(Default)]
pub struct StreamTextTracker {
    /// Bytes of the raw input whose stripped form is final.
    frozen_input: usize,
    /// Stripped (pre-truncation) output of the frozen prefix.
    frozen_stripped: String,
}

impl StreamTextTracker {
    /// Same contract as `stream_visible_text(full)`; `full` must be the
    /// accumulated response so far (append-only across calls).
    pub fn visible(&mut self, full: &str) -> String {
        self.advance(full);
        let hot = &full[self.frozen_input..];
        let mut stripped = self.frozen_stripped.clone();
        if !hot.is_empty() {
            stripped.push_str(&strip_stream_blocks(hot));
        }
        truncate_visible(stripped)
    }

    /// Move the frozen boundary forward while the stripped output stays
    /// provably final. Freezing rules (each mirrors one strip pass; the
    /// pass order is XML strip → thinking strip → legacy strip):
    ///
    /// - Prose without `<`/`[` is untouched by every pass.
    /// - `<tool_use>…</tool_use>` complete at the boundary: pass 1 runs
    ///   first on the raw text, so the match is stable under more input.
    /// - `<thinking>…</thinking>` complete at the boundary: stable only if
    ///   it contains no `<tool_use>` opener — pass 1 could otherwise eat
    ///   our closing tag once a later `</tool_use>` arrives.
    /// - `[TOOL:…] {…}` complete at the boundary: stable only if it
    ///   contains no `<` — passes 1–2 could otherwise rewrite its interior
    ///   before pass 3 sees it.
    /// - A lone `<` or `[` whose following bytes provably diverge from
    ///   every opener this pipeline strips (`<tool_use>`, `<thinking>`,
    ///   `[TOOL:`) is prose. The divergence window must itself be free of
    ///   `<` — a stripped block inside it could otherwise splice an opener
    ///   together (e.g. `<thi` + stripped block + `nking>` becomes a real
    ///   `<thinking>` after pass 1 removes the block).
    /// - Anything else — an unclosed block, an undecidably short tail —
    ///   stops the boundary; the tail is rescanned while it stays hot.
    fn advance(&mut self, full: &str) {
        loop {
            let hot = &full[self.frozen_input..];
            let Some(danger) = hot.find(['<', '[']) else {
                self.frozen_stripped.push_str(hot);
                self.frozen_input = full.len();
                return;
            };
            // Prose before the first `<`/`[` survives every pass as-is.
            self.frozen_stripped.push_str(&hot[..danger]);
            self.frozen_input += danger;
            let hot = &full[self.frozen_input..];

            let stripped_block_len = if hot.starts_with("<tool_use>") {
                match_len_at_start(&STRIP_TOOL_XML_RE, hot)
            } else if hot.starts_with("<thinking>") {
                match_len_at_start(&STRIP_THINKING_RE, hot)
                    .filter(|&len| !hot[..len].contains("<tool_use>"))
            } else if hot.starts_with("[TOOL:") {
                match_len_at_start(&STRIP_LEGACY_RE, hot).filter(|&len| !hot[..len].contains('<'))
            } else {
                None
            };

            if let Some(len) = stripped_block_len {
                // A stripped block contributes nothing to the output.
                self.frozen_input += len;
                continue;
            }

            // Not a complete strippable block. If the `<`/`[` provably
            // can't start one even with more input, it's inert prose for
            // the strip passes (global truncation still sees it).
            if inert_for_stripping(hot) {
                let danger_char_len = 1; // '<' and '[' are one byte
                self.frozen_stripped.push_str(&hot[..danger_char_len]);
                self.frozen_input += danger_char_len;
                continue;
            }

            return;
        }
    }
}

/// Whether the `<`/`[` at the start of `hot` provably can never become a
/// strippable opener, no matter what arrives later. Requires enough bytes
/// to decide, and a `<`-free decision window (a stripped block starting
/// inside the window could splice an opener together across its seam).
/// Byte-based: the openers are ASCII, and byte indexing stays safe when
/// multi-byte text follows the danger character.
fn inert_for_stripping(hot: &str) -> bool {
    const OPENERS: [&[u8]; 3] = [b"<tool_use>", b"<thinking>", b"[TOOL:"];
    let bytes = hot.as_bytes();
    let Some(window) = bytes.get(1..10) else {
        // Too short to rule every opener out yet.
        return false;
    };
    if window.contains(&b'<') {
        return false;
    }
    OPENERS.iter().all(|opener| !bytes.starts_with(opener))
}

/// Length of a `re` match starting exactly at the beginning of `text`.
fn match_len_at_start(re: &Regex, text: &str) -> Option<usize> {
    re.find(text).filter(|m| m.start() == 0).map(|m| m.end())
}

/// The three strip passes shared by [`stream_visible_text`] and
/// [`StreamTextTracker`] — pass order is load-bearing (see the tracker's
/// freezing rules).
fn strip_stream_blocks(text: &str) -> String {
    let without_xml = STRIP_TOOL_XML_RE.replace_all(text, "");
    let without_thinking = STRIP_THINKING_RE.replace_all(&without_xml, "");
    STRIP_LEGACY_RE
        .replace_all(&without_thinking, "")
        .into_owned()
}

/// Truncate stripped text at the first bare `<` or partial tool marker.
fn truncate_visible(mut visible: String) -> String {
    if let Some(index) = visible.find('<') {
        visible.truncate(index);
    }

    let cut_markers = [
        "<tool", "</tool", "<name", "</name", "<arg", "</arg", "[TOOL:",
    ];

    if let Some(index) = cut_markers
        .iter()
        .filter_map(|marker| visible.find(marker))
        .min()
    {
        visible.truncate(index);
    }

    visible
}

/// One-shot reference implementation of the visible-text pipeline.
/// Production streaming goes through [`StreamTextTracker`] (byte-identical
/// output, incremental cost); this stays as the ground truth the
/// equivalence test compares against.
#[cfg(test)]
pub fn stream_visible_text(text: &str) -> String {
    truncate_visible(strip_stream_blocks(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_tool_call() {
        let text = r#"Here is the result: [TOOL:bash] {"command": "ls -la"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls -la");
    }

    #[test]
    fn test_parse_mcp_tool_call() {
        let text =
            r#"[TOOL:mcp__github__create_issue] {"title": "Bug report", "body": "Found a bug"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "mcp__github__create_issue");
        assert_eq!(calls[0].arguments["title"], "Bug report");
    }

    #[test]
    fn test_no_tool_calls() {
        let text = "This is just a regular response without any tools.";
        assert!(parse_tool_calls(text).is_empty());
    }

    #[test]
    fn test_multiple_tool_calls() {
        let text = r#"[TOOL:bash] {"command": "pwd"}
Then [TOOL:file.read] {"path": "test.txt"}"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn test_parse_xml_tool_call() {
        let text = r#"
<tool_use>
<name>powershell</name>
<arguments>
{"command":"Get-Location"}
</arguments>
</tool_use>
"#;
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(calls[0].arguments["command"], "Get-Location");
    }

    /// The exact failure from the frozen-agent report: the model swapped the
    /// `</tool_use>` and `</arguments>` closing tags. Old parser dropped it
    /// with "Failed to parse <tool_use> body"; now the arguments are recovered.
    #[test]
    fn test_parse_xml_tool_call_with_swapped_closing_tags() {
        let text = "<tool_use>\n<name>powershell</name>\n<arguments>\n{\"command\":\"Get-Location\"}\n</tool_use>\n</arguments>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1, "errors: {errors:?}");
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(calls[0].arguments["command"], "Get-Location");
    }

    /// Genuinely broken JSON (unescaped quotes around a Windows path, the other
    /// half of the report) must NOT be silently dropped — it becomes an error
    /// the runner can feed back, and yields zero calls (never a wrong command).
    #[test]
    fn test_malformed_json_arguments_report_error_not_call() {
        let text = "<tool_use>\n<name>powershell</name>\n<arguments>\n{\"command\": \"Start-Process \"C:\\Users\\Aver\\x.png\"\"}\n</arguments>\n</tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert!(
            calls.is_empty(),
            "should not fabricate a call from bad JSON"
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("powershell"));
        assert!(errors[0].contains("not valid JSON"));
    }

    /// A dropped `<arguments>` wrapper still recovers the JSON object.
    #[test]
    fn test_parse_xml_tool_call_without_arguments_wrapper() {
        let text = "<tool_use>\n<name>bash</name>\n{\"command\":\"ls\"}\n</tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1, "errors: {errors:?}");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "ls");
    }

    /// The bare-JSON `{"name":…,"arguments":…}` shape (no XML tags) parses too.
    #[test]
    fn test_parse_bare_json_name_arguments_shape() {
        let text = "<tool_use>{\"name\":\"bash\",\"arguments\":{\"command\":\"pwd\"}}</tool_use>";
        let (calls, _errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments["command"], "pwd");
    }

    #[test]
    fn test_well_formed_call_produces_no_errors() {
        let text =
            "<tool_use><name>bash</name><arguments>{\"command\":\"ls\"}</arguments></tool_use>";
        let (calls, errors) = parse_tool_calls_with_errors(text);
        assert_eq!(calls.len(), 1);
        assert!(errors.is_empty());
    }

    #[test]
    fn test_strip_tool_calls() {
        let text = "Before\n<tool_use><name>bash</name><arguments>{\"command\":\"pwd\"}</arguments></tool_use>\nAfter";
        assert_eq!(strip_tool_calls(text), "Before\n\nAfter");
    }

    #[test]
    fn test_stream_visible_text_hides_incomplete_tool_use() {
        let text = "Answer first\n<tool_use><name>bash</name>";
        assert_eq!(stream_visible_text(text), "Answer first\n");
    }

    #[test]
    fn test_stream_visible_text_hides_partial_tool_tag() {
        let text = "Answer first\n<tool_use";
        assert_eq!(stream_visible_text(text), "Answer first\n");
    }

    /// The incremental tracker must be byte-identical to the full pipeline
    /// at EVERY prefix — the runner streams deltas by diffing consecutive
    /// outputs, so any divergence shows up as corrupted visible text.
    /// Cases target the freezing rules: seam-spanning blocks, openers and
    /// cut markers spliced together by a stripped block, unsafe interiors.
    #[test]
    fn tracker_matches_full_pipeline_on_every_prefix() {
        let cases: &[&str] = &[
            // Plain prose, multi-byte text, markdown links and brackets.
            "A long plain answer with several sentences and no markup at all.",
            "Многострочный ответ по-русски: если a < б, то ответ №1 🎉 готов.",
            "See [the docs](https://example.com) and [[wiki]] style links.",
            "array[index] and a[0] = b[1]; also [x] alone",
            // Bare '<' and C++-style shifts.
            "compare a < b and then continue with more prose after it",
            "std::cout << value << std::endl; more text",
            // Complete blocks followed by prose (the common agent turn).
            "Before <tool_use><name>bash</name><arguments>{\"command\":\"ls\"}</arguments></tool_use> after",
            "Before <thinking>secret reasoning</thinking> visible after",
            "Head [TOOL:bash] {\"command\":\"pwd\"} tail prose",
            // Unclosed blocks streaming in.
            "Answer first\n<tool_use><name>bash</name>",
            "Answer\n<thinking>still thinking",
            "Partial legacy [TOOL:bash] {\"command\":",
            // Thinking block whose interior hides a tool_use opener (unsafe
            // to freeze: pass 1 eats the closing tag once </tool_use> lands).
            "a <thinking> x <tool_use> y </thinking> stuff </tool_use> tail",
            // Openers spliced together across a stripped block's seam.
            "<think<tool_use>z</tool_use>ing>secret</thinking> visible",
            "[T<thinking>x</thinking>OOL:bash] {\"a\":1} tail",
            // Cut marker spliced together by a stripped legacy block.
            "a[[TOOL:x] {\"y\":1}TOOL:rest",
            // Legacy block with '<' inside its braces (unsafe to freeze).
            "[TOOL:x] {\"a\":\"<thinking>b\"} c</thinking> tail",
            // Multiple blocks back to back.
            "one <thinking>t1</thinking> two <tool_use><name>a</name><arguments>{}</arguments></tool_use> three [TOOL:b] {} four",
        ];

        for case in cases {
            let mut tracker = StreamTextTracker::default();
            let mut prefix_end = 0;
            while prefix_end < case.len() {
                prefix_end += 1;
                if !case.is_char_boundary(prefix_end) {
                    continue;
                }
                let prefix = &case[..prefix_end];
                assert_eq!(
                    tracker.visible(prefix),
                    stream_visible_text(prefix),
                    "tracker diverged from full pipeline at prefix {prefix:?} of case {case:?}"
                );
            }
        }
    }

    /// Freezing must actually advance past resolved constructs — otherwise
    /// the tracker silently degrades to the old O(n²) full rescan.
    #[test]
    fn tracker_freezes_past_resolved_constructs() {
        let mut tracker = StreamTextTracker::default();
        let text = "prose <thinking>done</thinking> tail with a < b and [link] more";
        tracker.visible(text);
        // Everything up to (at least) the bare '<' of "a < b" must freeze;
        // the inert '<' and '[' freeze too once their windows decide.
        assert!(
            tracker.frozen_input >= text.find("a < b").unwrap(),
            "frozen_input={} did not advance past the resolved thinking block",
            tracker.frozen_input
        );
    }
}
