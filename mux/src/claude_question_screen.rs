//! Claude Code's AskUserQuestion dialog read from the pane's screen, for a
//! question Claude has not yet written to its transcript. Claude can write
//! the assistant message that asks it only once it is answered, so the
//! screen, with Claude reporting `waiting` for `input needed`, is then the
//! only record of it. A single single-choice question looks like this:
//!
//! ```text
//! ──────────────────────────────
//!  ☐ Determinism
//! │ f6c bounds memory but executes about 15% more instructions than f5d.
//! │ Which end state should I build?
//! ❯ 1. Keep determinism (Recommended)
//!      Keep the reparse at savepoints, then cut parse cost for everyone.
//!   2. Fuse across savepoints
//!      Carry each function parsed through every chain of the request.
//!   3. Type something.
//! ──────────────────────────────
//!   4. Chat about this
//! Enter to select · ↑/↓ to navigate · Esc to cancel
//! ```
//!
//! A form of several questions shows them as tabs on one header line, and a
//! multi-select question marks its options with checkboxes; both are left to
//! the transcript.

use crate::agent::AgentMetadata;
use crate::agent_admission::incarnation_id;
use crate::agent_approval::{
    AgentApprovalChoice, AgentApprovalQuestion, AgentApprovalRequest, AGENT_APPROVAL_SCHEMA,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

/// The prefix of the item ID of a question read from the screen, which has
/// no transcript tool call to name it.
pub const SCREEN_ITEM_PREFIX: &str = "claude-screen:";

const HEADER_MARK: char = '\u{2610}';
const BAR: char = '\u{2502}';
const FOOTER: &str = "Enter to select";
const TYPE_SOMETHING: &str = "Type something.";
/// Characters of a question's letters and digits that identify it.
const KEY_LENGTH: usize = 40;

/// A single-choice question as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenQuestion {
    pub header: Option<String>,
    /// The question's text, its wrapped lines joined by spaces.
    pub question: String,
    /// Each option's label and description.
    pub options: Vec<(String, Option<String>)>,
}

/// The key of a question's text, the same for its transcript text and the
/// screen's rendering of it: the first letters and digits, lowercased, so
/// that wrapping and Markdown rendering do not change it.
pub fn question_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(KEY_LENGTH)
        .collect()
}

/// A line without its indentation and box border.
fn content(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || c == BAR)
}

fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 3 && line.chars().all(|c| matches!(c, '\u{2500}'..='\u{257f}'))
}

/// A numbered option line: its number and label.
fn option(line: &str) -> Option<(usize, &str)> {
    let text = content(line);
    let text = text.strip_prefix('\u{276f}').unwrap_or(text).trim_start();
    let (number, label) = text.split_once(". ")?;
    let label = label.trim();
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) && !label.is_empty())
        .then(|| Some((number.parse().ok()?, label)))
        .flatten()
}

/// Reads a single single-choice question from the pane's visible lines.
pub fn parse(lines: &[String]) -> Option<ScreenQuestion> {
    let footer = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with(FOOTER))?;
    let header_line = lines[..footer]
        .iter()
        .rposition(|line| line.contains(HEADER_MARK))?;
    let header_text = &lines[header_line];
    // A form of several questions lists them all, with a Submit tab.
    if header_text.matches(HEADER_MARK).count() != 1 || header_text.contains("Submit") {
        return None;
    }
    let header = content(header_text)
        .trim_start_matches(HEADER_MARK)
        .trim()
        .to_string();
    let mut question = Vec::new();
    let mut options: Vec<(String, Vec<String>)> = Vec::new();
    for line in &lines[header_line + 1..footer] {
        if is_rule(line) {
            continue;
        }
        match option(line) {
            Some((number, label)) if number == options.len() + 1 => {
                if label == TYPE_SOMETHING {
                    break;
                }
                options.push((label.to_string(), Vec::new()));
            }
            _ => {
                let text = content(line);
                if text.is_empty() {
                    continue;
                }
                match options.last_mut() {
                    Some((_, description)) => description.push(text.to_string()),
                    None => question.push(text.to_string()),
                }
            }
        }
    }
    let multi_select = options
        .iter()
        .any(|(label, _)| label.starts_with('[') || label.starts_with('\u{2610}'));
    if options.len() < 2 || question.is_empty() || multi_select {
        return None;
    }
    Some(ScreenQuestion {
        header: (!header.is_empty()).then_some(header),
        question: question.join(" "),
        options: options
            .into_iter()
            .map(|(label, description)| {
                (
                    label,
                    (!description.is_empty()).then(|| description.join(" ")),
                )
            })
            .collect(),
    })
}

/// Whether the screen shows `request`'s question with the same options.
pub fn shows_request(lines: &[String], request: &AgentApprovalRequest) -> bool {
    parse(lines).is_some_and(|shown| {
        Some(question_key(&shown.question)) == request.prompt.as_deref().map(question_key)
            && shown
                .options
                .iter()
                .map(|(label, _)| label)
                .eq(request.choices.iter().map(|choice| &choice.label))
    })
}

/// The `user_question` approval request for a question read from the screen
/// in the given turn. Its ID covers the question and its options, so the
/// same question read again is the same request.
pub fn approval_request(
    question: &ScreenQuestion,
    metadata: &AgentMetadata,
    turn_id: &str,
    shown_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let incarnation = incarnation_id(metadata)?;
    let labels: Vec<&str> = question
        .options
        .iter()
        .map(|(label, _)| label.as_str())
        .collect();
    let request_key = format!(
        "claude-screen-question\0{}\0{incarnation}\0{turn_id}\0{}\0{}",
        metadata.agent_id,
        question_key(&question.question),
        labels.join("\0")
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let choices: Vec<AgentApprovalChoice> = question
        .options
        .iter()
        .enumerate()
        .map(|(index, (label, description))| AgentApprovalChoice {
            id: format!("option_{}", index + 1),
            label: label.clone(),
            description: description.clone(),
        })
        .collect();
    Some(AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: "user_question".to_string(),
        item_id: format!("{SCREEN_ITEM_PREFIX}{request_id}"),
        request_id,
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation,
        turn_id: turn_id.to_string(),
        observed_at: shown_at,
        prompt: Some(question.question.clone()),
        reason: question.header.clone(),
        command: None,
        cwd: None,
        questions: vec![AgentApprovalQuestion {
            index: 0,
            header: question.header.clone(),
            question: question.question.clone(),
            multi_select: false,
            options: choices.clone(),
        }],
        choices,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    /// The dialog on re-opus's screen on 2026-10-25, as Panetone captured
    /// it, with its long lines cut at 200 columns.
    const SCREEN: &str = "\
● Confirmed: the 27B extra instructions come from f6c reparsing each function at every savepoint.
────────────────────────────────────────────────────────────
 ☐ Determinism
│ f6c bounds memory but executes about 15% more instructions than f5d on small synthetic functions, because each chain reparses functions from what the previous savepoint saved. That reparse is what m
│ Which end state should I build?
❯ 1. Keep determinism (Recommended)
     Keep the reparse at savepoints, then cut parse cost for everyone: patch the pinned LLVM so parsing skips the redundant full verifier (about 11 of the 27 billion extra instructions). Output never
     tiny functions, less on real ones.
  2. Fuse across savepoints
     Carry each function parsed through every chain of the request, saving at each savepoint without reparsing. Instruction parity with f5d and memory still bounded, but output keeps depending on whet
     it does today.
  3. Both
     Fuse across savepoints for speed and also write use-list order at savepoints so a cached load reproduces the in-memory module exactly. Deterministic and no reparse, but the bitcode writer becomes
     likely costs more than the reparse.
  4. Type something.
────────────────────────────────────────────────────────────
  5. Chat about this
Enter to select · ↑/↓ to navigate · Esc to cancel";

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn reads_the_question_its_options_and_their_descriptions() {
        let question = parse(&lines(SCREEN)).unwrap();
        assert_eq!(question.header.as_deref(), Some("Determinism"));
        assert!(question.question.starts_with("f6c bounds memory"));
        assert!(question
            .question
            .ends_with("That reparse is what m Which end state should I build?"));
        assert_eq!(
            question
                .options
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            [
                "Keep determinism (Recommended)",
                "Fuse across savepoints",
                "Both"
            ]
        );
        assert!(question.options[2]
            .1
            .as_deref()
            .unwrap()
            .ends_with("likely costs more than the reparse."));
        // The transcript's text of the question has the same key.
        assert_eq!(
            question_key(&question.question),
            question_key("f6c bounds memory but executes about 15% more instructions")
        );
    }

    #[test]
    fn other_dialogs_and_forms_are_not_a_single_question() {
        let form = SCREEN.replace("☐ Determinism", "←  ☐ Determinism  ☐ Scope  ✔ Submit  →");
        assert_eq!(parse(&lines(&form)), None);
        let multi = SCREEN
            .replace("1. Keep", "1. [ ] Keep")
            .replace("2. Fuse", "2. [ ] Fuse")
            .replace("3. Both", "3. [ ] Both");
        assert_eq!(parse(&lines(&multi)), None);
        assert_eq!(parse(&lines("❯ 1. Yes\n  2. No\nEsc to cancel")), None);
        let closed = SCREEN.replace("Enter to select · ↑/↓ to navigate · Esc to cancel", "");
        assert_eq!(parse(&lines(&closed)), None);
    }
}
