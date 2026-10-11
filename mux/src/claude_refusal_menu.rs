//! Claude Code's "Session paused" menu, shown when a model's safeguards
//! refuse a response twice in one turn. It offers to switch to a fallback
//! model or to edit the prompt, and waits without a timeout.
//!
//! Claude records nothing about the menu while it is up: its transcript is
//! silent and its session record reports only `waiting` for `dialog open`,
//! which other dialogs share. The menu is therefore read from the pane's
//! screen, and answered with keys checked against the screen.

use crate::agent::AgentMetadata;
use crate::agent_admission::incarnation_id;
use crate::agent_approval::{
    AgentApprovalChoice, AgentApprovalRequest, AgentApprovalResolution, AGENT_APPROVAL_SCHEMA,
};
use crate::pane::Pane;
use anyhow::Context;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::convert::TryInto;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wakterm_term::{KeyCode, KeyModifiers};

/// The approval kind of the menu, and the IDs of its two choices.
pub const REFUSAL_MENU_KIND: &str = "claude_refusal_fallback";
pub const RETRY_FALLBACK: &str = "retry_fallback";
pub const EDIT_PROMPT: &str = "edit_prompt";

const TITLE: &str = "Session paused";
const SWITCH_PREFIX: &str = "1. Switch to";
const EDIT_PREFIX: &str = "2. Edit prompt and retry";
const FOCUS_MARKER: char = '❯';

/// The menu as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalMenu {
    /// The explanation between the title and the choices.
    pub body: String,
    /// The choices' labels, without their numbers: switch, then edit.
    pub labels: [String; 2],
    /// The index of the choice the cursor is on.
    pub focused: usize,
}

/// Reads the menu from a pane's visible lines, or None when they do not show
/// its title followed by exactly its two choices.
pub fn parse(lines: &[String]) -> Option<RefusalMenu> {
    let title = lines.iter().rposition(|line| line.contains(TITLE))?;
    let mut body = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut focused = None;
    for line in &lines[title + 1..] {
        let text = line.trim_matches(|c: char| c.is_whitespace() || c == '│');
        let (marked, choice) = match text.strip_prefix(FOCUS_MARKER) {
            Some(rest) => (true, rest.trim_start()),
            None => (false, text),
        };
        let expected = [SWITCH_PREFIX, EDIT_PREFIX].get(labels.len()).copied();
        if expected.is_some_and(|prefix| choice.starts_with(prefix)) {
            if marked {
                focused = Some(labels.len());
            }
            labels.push(choice[3..].to_string());
        } else if labels.is_empty() {
            if !text.is_empty() {
                body.push(text);
            }
        } else if labels.len() == 2 {
            break;
        } else {
            // Something other than the second choice follows the first.
            return None;
        }
    }
    Some(RefusalMenu {
        body: body.join(" "),
        labels: labels.try_into().ok()?,
        focused: focused?,
    })
}

/// The pane's visible lines.
pub fn screen(pane: &Arc<dyn Pane>) -> Vec<String> {
    let dims = pane.get_dimensions();
    let top = dims.physical_top;
    let (_, lines) = pane.get_lines(top..top + dims.viewport_rows as isize);
    lines
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect()
}

/// The approval request for a menu shown in the given turn. Its ID covers
/// the menu's text, so the same menu read again is the same request.
pub fn approval_request(
    menu: &RefusalMenu,
    metadata: &AgentMetadata,
    turn_id: &str,
    observed_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let incarnation = incarnation_id(metadata)?;
    let request_key = format!(
        "claude-refusal-menu\0{}\0{incarnation}\0{turn_id}\0{}\0{}",
        metadata.agent_id, menu.labels[0], menu.labels[1]
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let choice = |id: &str, label: &str| AgentApprovalChoice {
        id: id.to_string(),
        label: label.to_string(),
        description: None,
    };
    Some(AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: REFUSAL_MENU_KIND.to_string(),
        item_id: request_id.clone(),
        request_id,
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation,
        turn_id: turn_id.to_string(),
        observed_at,
        prompt: Some(format!("{TITLE}: {}", menu.body)),
        reason: None,
        command: None,
        cwd: None,
        choices: vec![
            choice(RETRY_FALLBACK, &menu.labels[0]),
            choice(EDIT_PROMPT, &menu.labels[1]),
        ],
        questions: Vec::new(),
    })
}

/// Keys that answer the menu in a pane, checked against its screen. They
/// take up to seconds, so they run away from the main thread.
pub struct RefusalMenuKeys {
    pub(crate) pane: Arc<dyn Pane>,
    pub(crate) request: AgentApprovalRequest,
    pub(crate) choice_id: String,
}

const KEY_PAUSE: Duration = Duration::from_millis(150);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

impl RefusalMenuKeys {
    /// Answers the menu in the pane with the chosen option.
    pub fn run(self) -> anyhow::Result<AgentApprovalResolution> {
        let target = self
            .request
            .choices
            .iter()
            .position(|choice| choice.id == self.choice_id)
            .context("approval choice is not available")?;
        let labels = [
            self.request.choices[0].label.as_str(),
            self.request.choices[1].label.as_str(),
        ];
        answer(
            target,
            labels,
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

/// Moves the menu's cursor to row `target`, confirming on screen that it is
/// there, presses Enter, and waits for the menu to close. Stops without
/// pressing Enter when the screen does not show the menu with these labels.
fn answer(
    target: usize,
    labels: [&str; 2],
    read_screen: impl Fn() -> Vec<String>,
    mut press: impl FnMut(KeyCode) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for _ in 0..3 {
        let menu =
            parse(&read_screen()).context("the pane no longer shows the Session paused menu")?;
        anyhow::ensure!(
            menu.labels[0] == labels[0] && menu.labels[1] == labels[1],
            "the pane shows a different Session paused menu"
        );
        if menu.focused == target {
            press(KeyCode::Enter)?;
            let deadline = Instant::now() + CLOSE_TIMEOUT;
            while parse(&read_screen()).is_some() {
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "Enter was pressed, but the menu is still on screen"
                );
                std::thread::sleep(KEY_PAUSE);
            }
            return Ok(());
        }
        press(if menu.focused < target {
            KeyCode::DownArrow
        } else {
            KeyCode::UpArrow
        })?;
        std::thread::sleep(KEY_PAUSE);
    }
    anyhow::bail!("the menu's cursor did not move to the chosen row; nothing was confirmed")
}

#[cfg(test)]
mod test {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    const MENU: &str = "\
 some earlier output
╭──────────────────────────────────────────────╮
│ Session paused                               │
│                                              │
│ Opus 5.5's safeguards flagged this session.  │
│ Edit and retry, or continue with Opus 4.8.   │
│                                              │
│ Details: [cyber]                             │
│                                              │
│ ❯ 1. Switch to Opus 4.8                      │
│   2. Edit prompt and retry with Opus 5.5     │
╰──────────────────────────────────────────────╯
 Waiting for API response · will retry in 2m 40s";

    #[test]
    fn parses_the_menu_its_choices_and_the_focused_row() {
        let menu = parse(&lines(MENU)).unwrap();
        assert_eq!(
            menu.body,
            "Opus 5.5's safeguards flagged this session. Edit and retry, or continue with \
             Opus 4.8. Details: [cyber]"
        );
        assert_eq!(
            menu.labels,
            [
                "Switch to Opus 4.8".to_string(),
                "Edit prompt and retry with Opus 5.5".to_string()
            ]
        );
        assert_eq!(menu.focused, 0);

        let moved = MENU
            .replace("❯ 1.", "  1.")
            .replace("  2. Edit", "❯ 2. Edit");
        assert_eq!(parse(&lines(&moved)).unwrap().focused, 1);
    }

    #[test]
    fn other_screens_are_not_the_menu() {
        // The usage-credits dialog shares the title but not the choices.
        let credits = MENU
            .replace("1. Switch to Opus 4.8", "1. Continue on usage credits")
            .replace("2. Edit prompt and retry with Opus 5.5", "2. Switch models");
        assert_eq!(parse(&lines(&credits)), None);
        // Only one of the two choices, or no cursor, is not a menu to answer.
        let one = MENU.replace("│   2. Edit prompt and retry with Opus 5.5     │\n", "");
        assert_eq!(parse(&lines(&one)), None);
        assert_eq!(parse(&lines(&MENU.replace('❯', " "))), None);
        assert_eq!(parse(&lines("just a prompt\n> ")), None);
    }

    #[test]
    fn the_same_menu_is_the_same_request() {
        let metadata = AgentMetadata {
            agent_id: "agent".to_string(),
            name: "claude".to_string(),
            launch_cmd: "claude".to_string(),
            declared_cwd: "/tmp".to_string(),
            adopted_pid: Some(42),
            adopted_start_time: Some(99),
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            launch_supervisor: None,
            codex_app_server: None,
        };
        let menu = parse(&lines(MENU)).unwrap();
        let request = approval_request(&menu, &metadata, "turn-1", Utc::now()).unwrap();
        assert_eq!(request.kind, REFUSAL_MENU_KIND);
        assert_eq!(
            request
                .choices
                .iter()
                .map(|choice| (choice.id.as_str(), choice.label.as_str()))
                .collect::<Vec<_>>(),
            [
                (RETRY_FALLBACK, "Switch to Opus 4.8"),
                (EDIT_PROMPT, "Edit prompt and retry with Opus 5.5")
            ]
        );
        assert!(request
            .prompt
            .unwrap()
            .starts_with("Session paused: Opus 5.5"));
        let again = approval_request(&menu, &metadata, "turn-1", Utc::now()).unwrap();
        assert_eq!(again.request_id, request.request_id);
        let next_turn = approval_request(&menu, &metadata, "turn-2", Utc::now()).unwrap();
        assert_ne!(next_turn.request_id, request.request_id);
    }

    /// A menu that moves its cursor on arrows and closes on Enter, recording
    /// the keys it receives.
    struct FakeMenu {
        focused: std::cell::Cell<Option<usize>>,
        keys: std::cell::RefCell<Vec<KeyCode>>,
        arrows_work: bool,
    }

    impl FakeMenu {
        fn new(focused: usize, arrows_work: bool) -> Self {
            Self {
                focused: std::cell::Cell::new(Some(focused)),
                keys: Default::default(),
                arrows_work,
            }
        }

        fn screen(&self) -> Vec<String> {
            match self.focused.get() {
                None => lines("> "),
                Some(0) => lines(MENU),
                Some(_) => lines(
                    &MENU
                        .replace("❯ 1.", "  1.")
                        .replace("  2. Edit", "❯ 2. Edit"),
                ),
            }
        }

        fn press(&self, key: KeyCode) -> anyhow::Result<()> {
            self.keys.borrow_mut().push(key.clone());
            match key {
                KeyCode::Enter => self.focused.set(None),
                KeyCode::DownArrow if self.arrows_work => self.focused.set(Some(1)),
                KeyCode::UpArrow if self.arrows_work => self.focused.set(Some(0)),
                _ => {}
            }
            Ok(())
        }
    }

    const LABELS: [&str; 2] = ["Switch to Opus 4.8", "Edit prompt and retry with Opus 5.5"];

    #[test]
    fn answering_moves_to_the_chosen_row_before_enter() {
        // The focused first choice needs only Enter.
        let menu = FakeMenu::new(0, true);
        answer(0, LABELS, || menu.screen(), |key| menu.press(key)).unwrap();
        assert_eq!(*menu.keys.borrow(), [KeyCode::Enter]);

        // The second choice is reached with Down, and the first with Up.
        let menu = FakeMenu::new(0, true);
        answer(1, LABELS, || menu.screen(), |key| menu.press(key)).unwrap();
        assert_eq!(*menu.keys.borrow(), [KeyCode::DownArrow, KeyCode::Enter]);
        let menu = FakeMenu::new(1, true);
        answer(0, LABELS, || menu.screen(), |key| menu.press(key)).unwrap();
        assert_eq!(*menu.keys.borrow(), [KeyCode::UpArrow, KeyCode::Enter]);
    }

    #[test]
    fn answering_never_confirms_the_wrong_row_or_another_menu() {
        // Arrows that do not move the cursor: Enter is never pressed.
        let menu = FakeMenu::new(0, false);
        assert!(answer(1, LABELS, || menu.screen(), |key| menu.press(key)).is_err());
        assert!(!menu.keys.borrow().contains(&KeyCode::Enter));

        // A menu with other labels, or no menu, gets no keys at all.
        let menu = FakeMenu::new(0, true);
        let other = ["Switch to Opus 5", "Edit prompt and retry with Opus 5.5"];
        assert!(answer(0, other, || menu.screen(), |key| menu.press(key)).is_err());
        assert!(menu.keys.borrow().is_empty());
        let gone = FakeMenu::new(0, true);
        gone.focused.set(None);
        assert!(answer(0, LABELS, || gone.screen(), |key| gone.press(key)).is_err());
        assert!(gone.keys.borrow().is_empty());
    }
}
