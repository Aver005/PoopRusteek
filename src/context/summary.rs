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

/// Section headings the reply must contain, in order. Checked literally — a
/// truncated reply is rejected rather than half-used.
const REQUIRED_SECTIONS: [&str; 5] = [
    "## Objective",
    "## Important Details",
    "## Work State",
    "## Next Move",
    "## Relevant Files",
];

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

/// Accept a reply only when every section survived. A model that ran out of
/// output tokens mid-form fails here instead of becoming the whole context.
pub fn validate(reply: &str) -> Result<String, SummaryError> {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return Err(SummaryError::Empty);
    }
    for section in REQUIRED_SECTIONS {
        if !trimmed.contains(section) {
            return Err(SummaryError::MissingSection(section));
        }
    }
    Ok(trimmed.to_string())
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

    #[test]
    fn a_truncated_reply_is_refused_rather_than_half_used() {
        let full = "## Objective\na\n## Important Details\nb\n## Work State\nc\n## Next Move\nd\n## Relevant Files\ne";
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
