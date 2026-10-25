//! Muse Code's permission prompt for a shell command, which replaces its
//! input box:
//!
//! ```text
//! Would you like to run the following command?
//!   $ ls
//!   Stage 1/1
//!   Current argv: ["ls"]
//! ⟩ 1. Allow this stage once (y)
//!   2. Always allow in this workspace: ls ... (p)
//!   3. Abort the entire command (esc)
//! ```
//!
//! Muse records the request in its session log under a `pending_action_id`,
//! which is the approval request's ID, and records `decision_applied` once it
//! is answered. Wakterm answers with the request's own keys: `y` to allow the
//! command once, Esc to abort it. Always allowing it persists a rule, so that
//! choice is left to the pane.

use crate::agent::AgentMetadata;
use crate::agent_admission::incarnation_id;
use crate::agent_approval::{
    AgentApprovalChoice, AgentApprovalRequest, AgentApprovalResolution, AGENT_APPROVAL_SCHEMA,
};
use crate::pane::Pane;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wakterm_term::{KeyCode, KeyModifiers};

/// The approval kind of a permission prompt.
pub const PROMPT_KIND: &str = "muse_permission";
pub const ALLOW_ONCE: &str = "allow_once";
pub const DENY: &str = "deny";

const HEADING: &str = "Would you like to run the following command?";
const ALLOW_ROW: &str = "Allow this stage once (y)";

#[cfg(not(test))]
const DECISION_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const DECISION_TIMEOUT: Duration = Duration::from_millis(600);
const POLL: Duration = Duration::from_millis(100);

/// The approval request for permission request `action_id`, a shell command
/// that Muse asks to run in the given turn.
pub fn approval_request(
    metadata: &AgentMetadata,
    turn_id: &str,
    action_id: &str,
    command: &str,
    observed_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let choice = |id: &str, label: &str| AgentApprovalChoice {
        id: id.to_string(),
        label: label.to_string(),
        description: None,
    };
    Some(AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: PROMPT_KIND.to_string(),
        request_id: action_id.to_string(),
        item_id: action_id.to_string(),
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation_id(metadata)?,
        turn_id: turn_id.to_string(),
        observed_at,
        prompt: Some(format!("Muse asks to run: {command}")),
        reason: None,
        command: Some(command.to_string()),
        cwd: Some(metadata.declared_cwd.clone()),
        choices: vec![choice(ALLOW_ONCE, "Allow once"), choice(DENY, "Reject")],
        questions: Vec::new(),
    })
}

/// Whether the screen shows the prompt for `command`.
pub fn shows_prompt(lines: &[String], command: &str) -> bool {
    let first = command.lines().next().unwrap_or_default().trim();
    let has = |wanted: &str| lines.iter().any(|line| line.trim() == wanted);
    has(HEADING)
        && has(&format!("$ {first}"))
        && lines
            .iter()
            .any(|line| line.trim_end().ends_with(ALLOW_ROW))
}

/// The key that answers the prompt with `choice_id`.
fn key_for(choice_id: &str) -> anyhow::Result<KeyCode> {
    match choice_id {
        ALLOW_ONCE => Ok(KeyCode::Char('y')),
        DENY => Ok(KeyCode::Escape),
        _ => anyhow::bail!("approval choice is not available"),
    }
}

/// The key that answers the prompt in a pane, then a wait for Muse to log
/// the decision. The wait takes up to seconds, so it runs away from the
/// main thread.
pub struct PromptKeys {
    pub(crate) pane: Arc<dyn Pane>,
    pub(crate) log: PathBuf,
    pub(crate) request: AgentApprovalRequest,
    pub(crate) choice_id: String,
}

impl PromptKeys {
    pub fn run(self) -> anyhow::Result<AgentApprovalResolution> {
        let command = self.request.command.clone().unwrap_or_default();
        answer(
            &self.choice_id,
            || crate::claude_menu::screen(&self.pane),
            || crate::muse::approval_pending(&self.log, &self.request.item_id),
            |key| self.pane.key_down(key, KeyModifiers::NONE),
            &command,
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

/// Presses the choice's key while the screen shows the prompt for `command`
/// and the log shows the request pending, then waits for the log to record
/// the decision. Esc during a turn without the prompt would interrupt the
/// turn, so nothing is pressed unless both checks pass.
fn answer(
    choice_id: &str,
    read_screen: impl Fn() -> Vec<String>,
    pending: impl Fn() -> anyhow::Result<bool>,
    press: impl FnOnce(KeyCode) -> anyhow::Result<()>,
    command: &str,
) -> anyhow::Result<()> {
    let key = key_for(choice_id)?;
    anyhow::ensure!(pending()?, "approval request is no longer pending");
    anyhow::ensure!(
        shows_prompt(&read_screen(), command),
        "the pane does not show Muse's prompt for this command"
    );
    press(key)?;
    let deadline = Instant::now() + DECISION_TIMEOUT;
    while pending()? {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the key was pressed, but Muse has not recorded a decision after {} s; check the \
             pane before answering again",
            DECISION_TIMEOUT.as_secs_f32()
        );
        std::thread::sleep(POLL);
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// The prompt as Muse 1.4.12 draws it.
    const PROMPT: &str = "\
◆ Calling tools (5s · esc to interrupt)
────────────────────
Would you like to run the following command?
  $ ls
  Stage 1/1
  Current argv: [\"ls\"]
⟩ 1. Allow this stage once (y)
  2. Always allow in this workspace: ls ... (p)
  3. Abort the entire command (esc)
Change approval defaults in /permissions
────────────────────
  muse-wonder-1.0 · high · /code/x";

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn the_prompt_is_recognized_for_its_command() {
        assert!(shows_prompt(&lines(PROMPT), "ls"));
        assert!(!shows_prompt(&lines(PROMPT), "rm -rf x"));
        assert!(!shows_prompt(&lines("❯ ls"), "ls"));
    }

    #[test]
    fn answering_presses_the_key_and_waits_for_the_decision() {
        let pressed = RefCell::new(Vec::new());
        let polls = Cell::new(0);
        answer(
            DENY,
            || lines(PROMPT),
            || {
                polls.set(polls.get() + 1);
                Ok(polls.get() < 3)
            },
            |key| {
                pressed.borrow_mut().push(key);
                Ok(())
            },
            "ls",
        )
        .unwrap();
        assert_eq!(*pressed.borrow(), [KeyCode::Escape]);

        // Without the prompt on screen, or once decided, nothing is pressed.
        for (screen, pending) in [("❯ ", true), (PROMPT, false)] {
            let pressed = RefCell::new(Vec::new());
            assert!(answer(
                ALLOW_ONCE,
                || lines(screen),
                || Ok(pending),
                |key| {
                    pressed.borrow_mut().push(key);
                    Ok(())
                },
                "ls",
            )
            .is_err());
            assert!(pressed.borrow().is_empty());
        }

        // A key that Muse never answers to is reported, not retried.
        let pressed = RefCell::new(Vec::new());
        let error = answer(
            ALLOW_ONCE,
            || lines(PROMPT),
            || Ok(true),
            |key| {
                pressed.borrow_mut().push(key);
                Ok(())
            },
            "ls",
        )
        .unwrap_err();
        assert_eq!(*pressed.borrow(), [KeyCode::Char('y')]);
        assert!(error.to_string().contains("has not recorded a decision"));
    }
}
