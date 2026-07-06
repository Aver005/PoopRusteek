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
    pub name: String,
    pub arguments: Value,
}

pub fn parse_tool_calls(text: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    for cap in XML_TOOL_RE.captures_iter(text) {
        let body = cap[1].trim();

        if let (Some(name_cap), Some(args_cap)) =
            (XML_NAME_RE.captures(body), XML_ARGS_RE.captures(body))
        {
            let name = name_cap[1].trim().to_string();
            let args_str = args_cap[1].trim();
            match serde_json::from_str::<Value>(args_str) {
                Ok(arguments) => calls.push(ParsedToolCall { name, arguments }),
                Err(error) => tracing::warn!("Failed to parse <tool_use> arguments: {error}"),
            }
            continue;
        }

        match serde_json::from_str::<Value>(body) {
            Ok(value) => {
                let name = value
                    .get("tool")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let arguments = value
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                if let Some(name) = name {
                    calls.push(ParsedToolCall { name, arguments });
                }
            }
            Err(error) => tracing::warn!("Failed to parse <tool_use> body: {error}"),
        }
    }

    for cap in LEGACY_TOOL_RE.captures_iter(text) {
        let name = cap[1].to_string();
        let args_str = &cap[2];

        match serde_json::from_str::<Value>(args_str) {
            Ok(args) => {
                calls.push(ParsedToolCall {
                    name,
                    arguments: args,
                });
            }
            Err(e) => {
                tracing::warn!("Failed to parse tool arguments for '{name}': {e}");
            }
        }
    }

    calls
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
