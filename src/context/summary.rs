//! Rung 3: the summariser's prompt, its output check, and the facts we fill in
//! ourselves. See `.docs/context-compaction.md` §5.2 and §5.4 — the shape here
//! is not a matter of taste, it is what the measurements pointed at.
//!
//! Two rules drive everything below. **Ask for a form, never a ratio**: no
//! studied implementation asks a model to "compress five times", because models
//! do not obey it. **Never ask for a fact you can compute**: file lists and
//! commands are extracted from the tool calls, because every measured system
//! scored 2.19-2.45 out of 5 at remembering them.

use crate::provider::{ChatMessage, Role};

/// Tool results are cut to this before the summariser sees them. Smaller than
/// rung 0's capture limit on purpose: the summariser needs the shape of a
/// result, not its body.
const TOOL_RESULT_CHARS_FOR_SUMMARY: usize = 2_000;

/// Section headings the reply must contain, in this order. `## Work State` is
/// nothing without its three subsections, so they are part of the list.
const REQUIRED_SECTIONS: [&str; 8] = [
    "## Objective",
    "## Important Details",
    "## Work State",
    "### Completed",
    "### Active",
    "### Blocked",
    "## Next Move",
    "## Relevant Files",
];

/// The one section that must not be stacked when chunks are merged: several
/// "do this next" lists in a row are contradictory orders, not a summary.
const NEXT_MOVE: &str = "## Next Move";

/// A heading that only holds subsections, so it needs no `(none)` of its own.
const WORK_STATE: &str = "## Work State";

/// The form. Every section stays even when empty, because a fixed shape is far
/// easier for a small model than "summarise thoroughly".
const TEMPLATE: &str = r#"Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Objective
- [one or two sentences: what the user is trying to accomplish]

## Important Details
- [constraints, decisions and why, facts needed to continue, or "(none)"]

## Work State
### Completed
- [finished work and verified facts, or "(none)"]

### Active
- [work in progress or partial changes, or "(none)"]

### Blocked
- [blockers, failing commands, unknowns, or "(none)"]

## Next Move
1. [the immediate concrete action, or "(none)"]

## Relevant Files
- [path: why it matters, or "(none)"]
</template>

Rules:
- Keep every section, even when empty.
- Terse bullets, not prose paragraphs.
- Preserve exact file paths, symbols, commands, error strings and identifiers.
- Do not mention that context was compacted."#;

/// Carried forward when a summary already exists. Regenerating from scratch
/// loses standing constraints; merging keeps them (§5.2).
const UPDATE_RULES: &str = r#"The <prior-summary> covers everything before <conversation>. Produce one summary combining both — the prior one is discarded, so anything you do not carry over is lost.

- Carry forward objectives, constraints and user directives even when the conversation does not mention them again. Drop only what is finished.
- Where they conflict, the conversation wins: state the corrected fact and drop the old claim.
- Move finished work from Active to Completed."#;

pub struct SummaryRequest {
    pub system_prompt: String,
    pub user_prompt: String,
}

/// Flatten messages into a tag-free transcript. Tool calls become plain lines
/// so the summariser needs no tool schema — the same move Roo and opencode make.
fn render_transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages.iter().filter(|message| !message.ui_only) {
        let line = match message.role {
            Role::User => format!("[User]: {}", message.content),
            Role::Assistant => format!("[Assistant]: {}", message.content),
            Role::System => format!("[Note]: {}", message.content),
            Role::Tool => format!(
                "[Tool result: {}]: {}",
                message.name.as_deref().unwrap_or("unknown"),
                crate::util::truncate_with_ellipsis(
                    &message.content,
                    TOOL_RESULT_CHARS_FOR_SUMMARY
                )
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Build the call. `prior` is the summary this one replaces, if any.
pub fn build_request(messages: &[ChatMessage], prior: Option<&str>) -> SummaryRequest {
    let mut user_prompt = String::new();
    user_prompt.push_str("<conversation>\n");
    user_prompt.push_str(&render_transcript(messages));
    user_prompt.push_str("</conversation>\n\n");
    if let Some(prior) = prior.filter(|text| !text.trim().is_empty()) {
        user_prompt.push_str("<prior-summary>\n");
        user_prompt.push_str(prior.trim());
        user_prompt.push_str("\n</prior-summary>\n\n");
        user_prompt.push_str(UPDATE_RULES);
        user_prompt.push_str("\n\n");
    }
    user_prompt.push_str(TEMPLATE);
    SummaryRequest {
        system_prompt: "You summarise a coding session so another agent can continue it. \
             Follow the requested structure exactly and output nothing else."
            .to_string(),
        user_prompt,
    }
}

/// Why a reply was refused. The caller keeps the history exactly as it was and
/// reports the reason, rather than accepting a half-written summary as the
/// whole context — the defect still open in Goose (§6).
#[derive(Debug, PartialEq, Eq)]
pub enum SummaryError {
    Empty,
    MissingSection(&'static str),
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the summariser returned nothing"),
            Self::MissingSection(name) => {
                write!(f, "the summary is missing its `{name}` section")
            }
        }
    }
}

/// Accept a reply only when every section survived, in the order the template
/// asks for. Each search starts where the previous heading ended, so a
/// reordered or half-written form fails instead of becoming the whole context.
pub fn validate(reply: &str) -> Result<String, SummaryError> {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return Err(SummaryError::Empty);
    }
    let mut position = 0;
    for section in REQUIRED_SECTIONS {
        match trimmed[position..].find(section) {
            // Headings are ASCII, so the offset always lands on a boundary.
            Some(at) => position += at + section.len(),
            None => return Err(SummaryError::MissingSection(section)),
        }
    }
    Ok(trimmed.to_string())
}

/// Is this line the template's empty-section placeholder rather than content?
/// List markers are stripped first, so `- (none)` and `1. (none)` count too.
fn is_placeholder(line: &str) -> bool {
    let bare = line
        .trim_matches(|c: char| c == '-' || c == '*' || c == '.' || c.is_whitespace())
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_matches(|c: char| c == '.' || c.is_whitespace());
    bare.is_empty() || bare.eq_ignore_ascii_case("(none)") || bare.eq_ignore_ascii_case("none")
}

/// Split one summary into the lines under each [`REQUIRED_SECTIONS`] heading.
/// Lines under any other heading belong to no section and are dropped.
fn section_lines(text: &str) -> Vec<Vec<String>> {
    let mut sections = vec![Vec::new(); REQUIRED_SECTIONS.len()];
    let mut current = None;
    for line in text.lines() {
        let line = line.trim_end();
        let heading = line.trim();
        if let Some(index) = REQUIRED_SECTIONS.iter().position(|name| *name == heading) {
            current = Some(index);
        } else if heading.starts_with("##") {
            current = None;
        } else if let Some(index) = current
            && !heading.is_empty()
        {
            sections[index].push(line.to_string());
        }
    }
    sections
}

/// Fold mode 2's chunk summaries into a single instance of the form.
///
/// Mechanical rather than a second model call: a reduce pass would cost another
/// request and could reject everything after the chunks are already paid for.
pub fn merge(summaries: &[String]) -> String {
    if summaries.len() < 2 {
        return summaries.first().cloned().unwrap_or_default();
    }
    let parsed: Vec<Vec<Vec<String>>> = summaries.iter().map(|text| section_lines(text)).collect();
    let mut out = String::new();
    for (index, section) in REQUIRED_SECTIONS.iter().enumerate() {
        let mut lines: Vec<String> = Vec::new();
        // Chunks run oldest to newest, so the newest chunk's next move is the
        // live one; everything else accumulates in chronological order.
        let sources: Vec<&Vec<String>> = if *section == NEXT_MOVE {
            parsed
                .iter()
                .rev()
                .map(|chunk| &chunk[index])
                .find(|chunk| !chunk.iter().all(|line| is_placeholder(line)))
                .into_iter()
                .collect()
        } else {
            parsed.iter().map(|chunk| &chunk[index]).collect()
        };
        for line in sources.into_iter().flatten() {
            if !is_placeholder(line) && !lines.contains(line) {
                lines.push(line.clone());
            }
        }
        out.push_str(section);
        out.push('\n');
        if lines.is_empty() {
            if *section != WORK_STATE {
                out.push_str("- (none)\n");
            }
        } else {
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }
    out.trim_end().to_string()
}

/// Files touched and commands run, taken from the tool calls themselves. The
/// model is never asked for these (§5.4).
pub fn observed_facts(messages: &[ChatMessage]) -> String {
    let mut tools: Vec<&str> = Vec::new();
    for message in messages.iter().filter(|m| m.role == Role::Tool) {
        if let Some(name) = message.name.as_deref()
            && !tools.contains(&name)
        {
            tools.push(name);
        }
    }
    if tools.is_empty() {
        return String::new();
    }
    format!("\n\n## Tools Used\n- {}", tools.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("fix the parser"),
            ChatMessage::assistant("reading it"),
            ChatMessage::tool("call-1", &"log ".repeat(2_000)),
        ]
    }

    fn chunk(tag: &str, next_move: &str) -> String {
        format!(
            "## Objective\n- goal {tag}\n## Important Details\n(none)\n## Work State\n### Completed\n- done {tag}\n### Active\n(none)\n### Blocked\n(none)\n## Next Move\n1. {next_move}\n## Relevant Files\n- src/{tag}.rs: touched"
        )
    }

    #[test]
    fn merged_chunks_are_one_form_that_keeps_what_each_chunk_reported() {
        let merged = merge(&[
            chunk("a", "read the parser"),
            chunk("b", "run the tests"),
            chunk("c", "(none)"),
        ]);

        // What the next agent receives must be the form, not three of them.
        assert!(
            validate(&merged).is_ok(),
            "merged summary is not a form:\n{merged}"
        );
        for section in REQUIRED_SECTIONS {
            assert_eq!(
                merged.matches(section).count(),
                1,
                "`{section}` appears more than once:\n{merged}"
            );
        }
        for tag in ["a", "b", "c"] {
            assert!(
                merged.contains(&format!("src/{tag}.rs")),
                "chunk {tag} was lost:\n{merged}"
            );
        }
        // The newest chunk with a real next move wins; empties never stack up.
        assert!(
            merged.contains("run the tests"),
            "stale next move:\n{merged}"
        );
        assert!(
            !merged.contains("read the parser"),
            "two next moves:\n{merged}"
        );
        // Important Details, Active and Blocked were empty in every chunk:
        // one placeholder each, not one per chunk.
        assert_eq!(
            merged.matches("(none)").count(),
            3,
            "placeholders piled up:\n{merged}"
        );
    }

    #[test]
    fn a_single_chunk_is_passed_through_untouched() {
        let only = chunk("a", "carry on");
        assert_eq!(merge(std::slice::from_ref(&only)), only);
    }

    #[test]
    fn the_request_asks_for_a_form_and_never_for_a_ratio() {
        let request = build_request(&conversation(), None);
        assert!(request.user_prompt.contains("## Objective"));
        assert!(request.user_prompt.contains("(none)"));
        // A compression factor is exactly what models do not obey.
        assert!(!request.user_prompt.contains("times"));
        assert!(!request.user_prompt.to_lowercase().contains("compress"));
    }

    #[test]
    fn tool_results_are_cut_before_the_summariser_sees_them() {
        let request = build_request(&conversation(), None);
        let start = request.user_prompt.find("[Tool result").expect("rendered");
        let end = request.user_prompt[start..]
            .find("</conversation>")
            .expect("closed");
        assert!(
            end < TOOL_RESULT_CHARS_FOR_SUMMARY + 200,
            "tool result reached the summariser at {end} chars"
        );
    }

    #[test]
    fn a_prior_summary_brings_the_merge_rules_with_it() {
        let plain = build_request(&conversation(), None);
        assert!(!plain.user_prompt.contains("<prior-summary>"));

        let merged = build_request(&conversation(), Some("## Objective\nold goal"));
        assert!(merged.user_prompt.contains("<prior-summary>"));
        assert!(merged.user_prompt.contains("the conversation wins"));

        // An empty prior summary is not a prior summary.
        let blank = build_request(&conversation(), Some("   "));
        assert!(!blank.user_prompt.contains("<prior-summary>"));
    }

    const FILLED: &str = "## Objective\na\n## Important Details\nb\n## Work State\n### Completed\nc\n### Active\nd\n### Blocked\ne\n## Next Move\nf\n## Relevant Files\ng";

    #[test]
    fn a_work_state_without_its_subsections_is_not_a_work_state() {
        assert!(validate(FILLED).is_ok());
        let flat = "## Objective\na\n## Important Details\nb\n## Work State\nc\n## Next Move\nd\n## Relevant Files\ne";
        assert!(
            validate(flat).is_err(),
            "Completed/Active/Blocked are part of the form"
        );
    }

    #[test]
    fn the_form_is_refused_when_its_sections_arrive_out_of_order() {
        // The order carries meaning — the next agent reads it top to bottom —
        // and a model that reorders it has not followed the template.
        let shuffled = "## Next Move\nf\n## Objective\na\n## Important Details\nb\n## Work State\n### Completed\nc\n### Active\nd\n### Blocked\ne\n## Relevant Files\ng";
        assert!(validate(shuffled).is_err(), "order is not checked");
    }

    #[test]
    fn a_truncated_reply_is_refused_rather_than_half_used() {
        let full = FILLED;
        assert!(validate(full).is_ok());

        // Ran out of output tokens partway through the form.
        let cut = "## Objective\na\n## Important Details\nb\n## Work St";
        assert_eq!(
            validate(cut),
            Err(SummaryError::MissingSection("## Work State"))
        );
        assert_eq!(validate("   "), Err(SummaryError::Empty));
    }

    #[test]
    fn tools_are_read_from_the_history_not_asked_of_the_model() {
        let mut messages = conversation();
        messages.push(ChatMessage::tool("call-2", "more"));
        messages[2].name = Some("read_file".to_string());
        messages[3].name = Some("shell".to_string());
        let facts = observed_facts(&messages);
        assert!(facts.contains("read_file"));
        assert!(facts.contains("shell"));
        assert!(observed_facts(&messages[..2]).is_empty());
    }
}
