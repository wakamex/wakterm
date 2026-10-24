//! Devin's permission prompt, which asks before running a tool: rows such
//! as Allow, Accept edits mode, Bypass mode and Deny with a `❭` cursor on
//! one, the command it would run above them, and a footer of keys ending in
//! "↵ confirm · esc cancel". When the command can be granted by prefix, the
//! Allow row also shows a scope, Once, Session, Project or Always, that the
//! Left and Right keys change.
//!
//! Devin records nothing about a prompt while it is up: the tool call's rows
//! reach its session database only once the tool finishes. A prompt is
//! therefore read from the pane's screen, and answered with keys checked
//! against the screen after every press.

use crate::agent::AgentMetadata;
use crate::agent_admission::incarnation_id;
use crate::agent_approval::{
    AgentApprovalChoice, AgentApprovalRequest, AgentApprovalResolution, AGENT_APPROVAL_SCHEMA,
};
use crate::pane::Pane;
use anyhow::Context;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wakterm_term::{KeyCode, KeyModifiers};

/// The approval kind of a permission prompt.
pub const PROMPT_KIND: &str = "devin_permission";
/// Its choices: run the tool this once, or refuse it. Bypass mode and the
/// wider scopes change Devin's permissions beyond this request, so they are
/// left to the pane.
pub const ALLOW_ONCE: &str = "allow_once";
pub const DENY: &str = "deny";

const CURSOR: char = '\u{276d}';
const ROWS: [&str; 4] = ["Allow", "Accept edits mode", "Bypass mode", "Deny"];
const ONCE: &str = "Once";

/// A permission prompt as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevinPrompt {
    /// The command Devin asks to run, when the prompt shows one.
    pub command: Option<String>,
    /// The rows' labels, in order.
    pub labels: Vec<String>,
    /// The index of the row the cursor is on.
    pub focused: usize,
    /// The scope the Allow row shows, when the command can be granted by
    /// prefix.
    pub scope: Option<String>,
}

impl DevinPrompt {
    /// Whether this is the same prompt as `other`, wherever its cursor and
    /// scope are.
    pub fn same_prompt(&self, other: &DevinPrompt) -> bool {
        self.command == other.command && self.labels == other.labels
    }
}

/// A row's label and scope, from its line without the cursor.
fn row(line: &str) -> Option<(String, Option<String>)> {
    let text = line.trim_start_matches([CURSOR, ' ']).trim_end();
    let label = ROWS
        .iter()
        .filter(|label| text.starts_with(**label))
        .max_by_key(|label| label.len())?;
    let rest = text[label.len()..].trim();
    if rest.is_empty() {
        return Some((label.to_string(), None));
    }
    // The scope selector: "← ◼◼◼◼ → Once".
    let scope = rest.rsplit_once('\u{2192}')?.1.trim();
    (rest.starts_with('\u{2190}') && !scope.is_empty())
        .then(|| (label.to_string(), Some(scope.to_string())))
}

/// Reads the prompt from a pane's visible lines: the rows right above the
/// lowest footer that ends in "esc cancel", with the cursor on exactly one.
pub fn parse(lines: &[String]) -> Option<DevinPrompt> {
    let footer = lines.iter().rposition(|line| {
        let line = line.trim();
        line.contains("\u{21b5} confirm") && line.ends_with("esc cancel")
    })?;
    let first = (0..footer)
        .rev()
        .take_while(|&index| row(&lines[index]).is_some())
        .last()?;
    let mut labels = Vec::new();
    let mut focused = None;
    let mut scope = None;
    for (index, line) in lines[first..footer].iter().enumerate() {
        let (label, row_scope) = row(line)?;
        if line.trim_start().starts_with(CURSOR) {
            if focused.replace(index).is_some() {
                return None;
            }
        }
        if label == "Allow" {
            scope = row_scope;
        }
        labels.push(label);
    }
    let focused = focused?;
    let command = lines[..first].iter().rev().take(6).find_map(|line| {
        line.trim_start()
            .strip_prefix("\u{2514} $ ")
            .map(|command| command.trim().to_string())
    });
    (labels.len() >= 2).then_some(DevinPrompt {
        command,
        labels,
        focused,
        scope,
    })
}

/// The pane's visible lines.
pub fn screen(pane: &Arc<dyn Pane>) -> Vec<String> {
    crate::claude_menu::screen(pane)
}

/// The approval request for a prompt that appeared at `shown_at` in the
/// given turn. Its ID covers the prompt's text and when it appeared, so the
/// same prompt read again is the same request.
pub fn approval_request(
    prompt: &DevinPrompt,
    metadata: &AgentMetadata,
    turn_id: &str,
    shown_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let incarnation = incarnation_id(metadata)?;
    if !prompt.labels.iter().any(|label| label == "Allow")
        || !prompt.labels.iter().any(|label| label == "Deny")
    {
        return None;
    }
    let request_key = format!(
        "devin-permission\0{}\0{incarnation}\0{turn_id}\0{}\0{}\0{}",
        metadata.agent_id,
        shown_at.timestamp_micros(),
        prompt.command.as_deref().unwrap_or_default(),
        prompt.labels.join("\0")
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let choice = |id: &str, label: &str| AgentApprovalChoice {
        id: id.to_string(),
        label: label.to_string(),
        description: None,
    };
    Some(AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: PROMPT_KIND.to_string(),
        item_id: request_id.clone(),
        request_id,
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation,
        turn_id: turn_id.to_string(),
        observed_at: shown_at,
        prompt: Some(match &prompt.command {
            Some(command) => format!("Devin asks to run: {command}"),
            None => "Devin asks permission to use a tool".to_string(),
        }),
        reason: None,
        command: prompt.command.clone(),
        cwd: Some(metadata.declared_cwd.clone()),
        choices: vec![choice(ALLOW_ONCE, "Allow once"), choice(DENY, "Deny")],
        questions: Vec::new(),
    })
}

/// Keys that answer the prompt in a pane, checked against its screen. They
/// take up to seconds, so they run away from the main thread.
pub struct PromptKeys {
    pub(crate) pane: Arc<dyn Pane>,
    pub(crate) request: AgentApprovalRequest,
    pub(crate) choice_id: String,
}

const KEY_PAUSE: Duration = Duration::from_millis(150);
/// How many presses a move may take, which is more than any prompt's rows
/// or scopes.
const MAX_MOVES: usize = 8;
#[cfg(not(test))]
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const CLOSE_TIMEOUT: Duration = Duration::from_millis(600);
#[cfg(not(test))]
const REDRAW_PAUSE: Duration = KEY_PAUSE;
#[cfg(test)]
const REDRAW_PAUSE: Duration = Duration::from_millis(1);

impl PromptKeys {
    /// Answers the prompt in the pane with the chosen option.
    pub fn run(self) -> anyhow::Result<AgentApprovalResolution> {
        let target = match self.choice_id.as_str() {
            ALLOW_ONCE => "Allow",
            DENY => "Deny",
            _ => anyhow::bail!("approval choice is not available"),
        };
        answer(
            target,
            self.request.command.as_deref(),
            || screen(&self.pane),
            |key| self.pane.key_down(key, KeyModifiers::NONE),
        )?;
        Ok(AgentApprovalResolution {
            schema: AGENT_APPROVAL_SCHEMA.to_string(),
            request_id: self.request.request_id.clone(),
            agent_id: self.request.agent_id.clone(),
            incarnation_id: self.request.incarnation_id.clone(),
            choice_id: self.choice_id.clone(),
            resolved: true,
        })
    }
}

/// Moves the cursor to the row labelled `target`, sets the Allow row's
/// scope to Once, presses Enter, and waits for the prompt to close. Each
/// press is checked against the redrawn screen, and nothing more is pressed
/// once the screen shows something else than the prompt for `command`.
fn answer(
    target: &str,
    command: Option<&str>,
    read_screen: impl Fn() -> Vec<String>,
    mut press: impl FnMut(KeyCode) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let read = || -> anyhow::Result<DevinPrompt> {
        let prompt = parse(&read_screen()).context("the pane no longer shows the prompt")?;
        anyhow::ensure!(
            prompt.command.as_deref() == command,
            "the pane shows a prompt for another command"
        );
        Ok(prompt)
    };
    let mut prompt = read()?;
    let row = prompt
        .labels
        .iter()
        .position(|label| label == target)
        .with_context(|| format!("the prompt has no {target} row"))?;
    let mut moves = 0;
    while prompt.focused != row {
        anyhow::ensure!(
            moves < MAX_MOVES,
            "the cursor did not reach the {target} row"
        );
        press(if prompt.focused < row {
            KeyCode::DownArrow
        } else {
            KeyCode::UpArrow
        })?;
        moves += 1;
        std::thread::sleep(REDRAW_PAUSE);
        prompt = read()?;
    }
    if target == "Allow" {
        // The scope stops at Once going left.
        while prompt.scope.as_deref().is_some_and(|scope| scope != ONCE) {
            anyhow::ensure!(moves < MAX_MOVES, "the scope did not reach Once");
            press(KeyCode::LeftArrow)?;
            moves += 1;
            std::thread::sleep(REDRAW_PAUSE);
            prompt = read()?;
            anyhow::ensure!(prompt.focused == row, "the cursor left the Allow row");
        }
    }
    press(KeyCode::Enter)?;
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    while parse(&read_screen()).is_some_and(|shown| shown.same_prompt(&prompt)) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "Enter was pressed on {target}, but the prompt is still on screen after {} s; it \
             may still take the choice, so check the pane before answering again",
            CLOSE_TIMEOUT.as_secs()
        );
        std::thread::sleep(KEY_PAUSE);
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::cell::RefCell;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    /// The prompt for a command that can be granted by prefix.
    const SCOPED: &str = "\
 ⏺ Running command
 └ $ mkdir d1
❭ Allow       ← ◼◼◼◼ → Session
  Bypass mode
  Deny
↑↓ select · ←→ scope · e edit · ↵ confirm · esc cancel";

    /// The prompt for a command that cannot, with an extra row.
    const UNSCOPED: &str = "\
 ⏺ Running command
 └ $ echo x > f.txt && cat f.txt
❭ Allow
  Accept edits mode
  Bypass mode
  Deny
↑↓ select · e edit · ↵ confirm · esc cancel";

    #[test]
    fn parses_the_rows_cursor_scope_and_command() {
        let prompt = parse(&lines(SCOPED)).unwrap();
        assert_eq!(prompt.command.as_deref(), Some("mkdir d1"));
        assert_eq!(prompt.labels, ["Allow", "Bypass mode", "Deny"]);
        assert_eq!(prompt.focused, 0);
        assert_eq!(prompt.scope.as_deref(), Some("Session"));

        let prompt = parse(&lines(UNSCOPED)).unwrap();
        assert_eq!(
            prompt.labels,
            ["Allow", "Accept edits mode", "Bypass mode", "Deny"]
        );
        assert_eq!(prompt.scope, None);

        // The idle input box is not a prompt.
        assert_eq!(
            parse(&lines(
                "────\n❭ Ask Devin to build features, fix bugs, or work on your code\n────"
            )),
            None
        );
    }

    /// A fake Devin prompt that moves its cursor and scope with the keys
    /// pressed and closes on Enter.
    fn run(text: &str, target: &str) -> (anyhow::Result<()>, Vec<KeyCode>) {
        let prompt = RefCell::new(parse(&lines(text)));
        let pressed = RefCell::new(Vec::new());
        let scopes = ["Once", "Session", "Project", "Always"];
        let render = |prompt: &DevinPrompt| {
            let mut out = vec![format!(" └ $ {}", prompt.command.clone().unwrap())];
            for (index, label) in prompt.labels.iter().enumerate() {
                let mark = if index == prompt.focused {
                    "\u{276d} "
                } else {
                    "  "
                };
                let scope = match (&prompt.scope, label.as_str()) {
                    (Some(scope), "Allow") => format!("       \u{2190} \u{25fc} \u{2192} {scope}"),
                    _ => String::new(),
                };
                out.push(format!("{mark}{label}{scope}"));
            }
            out.push("\u{2191}\u{2193} select \u{b7} \u{21b5} confirm \u{b7} esc cancel".into());
            out
        };
        let command = parse(&lines(text)).unwrap().command;
        let result = answer(
            target,
            command.as_deref(),
            || prompt.borrow().as_ref().map(render).unwrap_or_default(),
            |key| {
                pressed.borrow_mut().push(key);
                let mut prompt = prompt.borrow_mut();
                let Some(shown) = prompt.as_mut() else {
                    return Ok(());
                };
                match key {
                    KeyCode::DownArrow => {
                        shown.focused = (shown.focused + 1).min(shown.labels.len() - 1)
                    }
                    KeyCode::UpArrow => shown.focused = shown.focused.saturating_sub(1),
                    KeyCode::LeftArrow => {
                        if let Some(scope) = shown.scope.as_mut() {
                            let index = scopes.iter().position(|s| s == scope).unwrap();
                            *scope = scopes[index.saturating_sub(1)].to_string();
                        }
                    }
                    KeyCode::Enter => *prompt = None,
                    _ => {}
                }
                Ok(())
            },
        );
        (result, pressed.into_inner())
    }

    #[test]
    fn allowing_sets_the_scope_to_once_and_denying_moves_to_its_row() {
        let (result, keys) = run(SCOPED, "Allow");
        result.unwrap();
        assert_eq!(keys, [KeyCode::LeftArrow, KeyCode::Enter]);

        let (result, keys) = run(SCOPED, "Deny");
        result.unwrap();
        assert_eq!(
            keys,
            [KeyCode::DownArrow, KeyCode::DownArrow, KeyCode::Enter]
        );

        let (result, keys) = run(UNSCOPED, "Deny");
        result.unwrap();
        assert_eq!(
            keys,
            [
                KeyCode::DownArrow,
                KeyCode::DownArrow,
                KeyCode::DownArrow,
                KeyCode::Enter
            ]
        );
    }

    #[test]
    fn presses_nothing_for_a_prompt_for_another_command() {
        let pressed = RefCell::new(Vec::new());
        let result = answer(
            "Allow",
            Some("rm -rf /tmp/x"),
            || lines(SCOPED),
            |key| {
                pressed.borrow_mut().push(key);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(pressed.into_inner().is_empty());
    }
}
