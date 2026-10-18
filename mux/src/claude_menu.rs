//! Claude Code's confirmation menus: a title and an explanation under a
//! rule, then numbered choices with a cursor on one of them. A slash command
//! raises some, such as "Switch model?", and Claude raises "Session paused"
//! when a model's safeguards refuse a response twice in one turn. Claude
//! waits on them without a timeout.
//!
//! Claude records nothing about a menu while it is up: its transcript is
//! silent and its session record reports only `waiting` for `dialog open`.
//! A menu is therefore read from the pane's screen, and answered with keys
//! checked against the screen.

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

/// The approval kind of a menu, whose choice IDs are the choices' numbers.
pub const MENU_KIND: &str = "claude_menu";
/// The approval kind of the "Session paused" menu that offers a fallback
/// model, and the IDs of its two choices.
pub const REFUSAL_MENU_KIND: &str = "claude_refusal_fallback";
pub const RETRY_FALLBACK: &str = "retry_fallback";
pub const EDIT_PROMPT: &str = "edit_prompt";

const REFUSAL_TITLE: &str = "Session paused";
const REFUSAL_LABEL_PREFIXES: [&str; 2] = ["Switch to", "Edit prompt and retry"];
const FOCUS_MARKER: char = '\u{276f}';
/// How far above its choices a menu's rule may be.
const MAX_HEADER_LINES: usize = 40;

/// A menu as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeMenu {
    pub title: String,
    /// The explanation between the title and the choices.
    pub body: String,
    /// The choices' labels, without their numbers, in order.
    pub labels: Vec<String>,
    /// The index of the choice the cursor is on.
    pub focused: usize,
}

impl ClaudeMenu {
    /// Whether this is the same menu as `other`, wherever its cursor is.
    pub fn same_menu(&self, other: &ClaudeMenu) -> bool {
        self.title == other.title && self.labels == other.labels
    }

    fn offers_fallback_model(&self) -> bool {
        self.title == REFUSAL_TITLE
            && self.labels.len() == REFUSAL_LABEL_PREFIXES.len()
            && self
                .labels
                .iter()
                .zip(REFUSAL_LABEL_PREFIXES)
                .all(|(label, prefix)| label.starts_with(prefix))
    }
}

/// A line without its indentation and box borders.
fn content(line: &str) -> &str {
    line.trim_matches(|c: char| c.is_whitespace() || c == '\u{2502}')
}

/// A line that only draws a rule or a box edge.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 3
        && line
            .chars()
            .all(|c| matches!(c, '\u{2500}'..='\u{257f}' | '\u{2594}'))
}

/// Splits a choice line into whether the cursor is on it, its number and
/// its label.
fn choice(line: &str) -> Option<(bool, usize, &str)> {
    let text = content(line);
    let (marked, text) = match text.strip_prefix(FOCUS_MARKER) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text),
    };
    let (number, label) = text.split_once(". ")?;
    let label = label.trim();
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) || label.is_empty() {
        return None;
    }
    Some((marked, number.parse().ok()?, label))
}

/// Reads the menu from a pane's visible lines: the lowest run of at least
/// two consecutive lines numbered from 1 with the cursor on exactly one,
/// below a title that follows a rule. None when the lines show no such menu.
pub fn parse(lines: &[String]) -> Option<ClaudeMenu> {
    let cursor = lines
        .iter()
        .rposition(|line| choice(line).is_some_and(|(marked, _, _)| marked))?;
    let (_, cursor_number, _) = choice(&lines[cursor])?;
    let first = cursor.checked_sub(cursor_number.checked_sub(1)?)?;
    let mut labels = Vec::new();
    for line in &lines[first..] {
        match choice(line) {
            Some((marked, number, label))
                if number == labels.len() + 1 && marked == (first + labels.len() == cursor) =>
            {
                labels.push(label.to_string())
            }
            _ => break,
        }
    }
    if labels.len() < 2 || first + labels.len() <= cursor {
        return None;
    }
    let rule = lines[..first]
        .iter()
        .rposition(|line| is_rule(line))
        .filter(|rule| first - rule <= MAX_HEADER_LINES)?;
    let mut header = lines[rule + 1..first]
        .iter()
        .map(|line| content(line))
        .filter(|text| !text.is_empty());
    let title = header.next()?.to_string();
    Some(ClaudeMenu {
        title,
        body: header.collect::<Vec<_>>().join(" "),
        labels,
        focused: cursor - first,
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

/// The approval request for a menu that appeared at `shown_at` in the given
/// turn. Its ID covers the menu's text and when it appeared, so the same
/// menu read again is the same request.
pub fn approval_request(
    menu: &ClaudeMenu,
    metadata: &AgentMetadata,
    turn_id: &str,
    shown_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let incarnation = incarnation_id(metadata)?;
    let request_key = format!(
        "claude-menu\0{}\0{incarnation}\0{turn_id}\0{}\0{}\0{}",
        metadata.agent_id,
        shown_at.timestamp_micros(),
        menu.title,
        menu.labels.join("\0")
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let fallback = menu.offers_fallback_model();
    let choices = menu
        .labels
        .iter()
        .enumerate()
        .map(|(index, label)| AgentApprovalChoice {
            id: if fallback {
                [RETRY_FALLBACK, EDIT_PROMPT][index].to_string()
            } else {
                (index + 1).to_string()
            },
            label: label.clone(),
            description: None,
        })
        .collect();
    Some(AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: if fallback {
            REFUSAL_MENU_KIND
        } else {
            MENU_KIND
        }
        .to_string(),
        item_id: request_id.clone(),
        request_id,
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation,
        turn_id: turn_id.to_string(),
        observed_at: shown_at,
        prompt: Some(if menu.body.is_empty() {
            menu.title.clone()
        } else {
            format!("{}: {}", menu.title, menu.body)
        }),
        reason: None,
        command: None,
        cwd: None,
        choices,
        questions: Vec::new(),
    })
}

/// Keys that answer the menu in a pane, checked against its screen. They
/// take up to seconds, so they run away from the main thread.
pub struct MenuKeys {
    pub(crate) pane: Arc<dyn Pane>,
    pub(crate) request: AgentApprovalRequest,
    pub(crate) choice_id: String,
}

const KEY_PAUSE: Duration = Duration::from_millis(150);
/// How long to wait for the menu to close after its row's number is pressed.
#[cfg(not(test))]
const CLOSE_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const CLOSE_TIMEOUT: Duration = Duration::from_millis(1500);

impl MenuKeys {
    /// Answers the menu in the pane with the chosen option.
    pub fn run(self) -> anyhow::Result<AgentApprovalResolution> {
        let target = self
            .request
            .choices
            .iter()
            .position(|choice| choice.id == self.choice_id)
            .context("approval choice is not available")?;
        let labels = self
            .request
            .choices
            .iter()
            .map(|choice| choice.label.as_str())
            .collect::<Vec<_>>();
        answer(
            target,
            &labels,
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

/// Chooses row `target` by pressing its number, which Claude's menus take
/// as choosing and confirming that row, and waits for the menu to close.
/// One key leaves no cursor to track, so a pane that redraws slowly cannot
/// make the choice land on another row. Presses nothing when the screen
/// does not show the menu with these labels. When the menu is still shown
/// after the wait, the choice may still be on its way, so nothing else is
/// pressed and the error says which number was.
fn answer(
    target: usize,
    labels: &[&str],
    read_screen: impl Fn() -> Vec<String>,
    mut press: impl FnMut(KeyCode) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let menu = parse(&read_screen()).context("the pane no longer shows the menu")?;
    anyhow::ensure!(menu.labels == labels, "the pane shows a different menu");
    let digit = <u32 as std::convert::TryFrom<usize>>::try_from(target + 1)
        .ok()
        .and_then(|number| char::from_digit(number, 10))
        .context("the chosen row has no number key")?;
    press(KeyCode::Char(digit))?;
    let deadline = Instant::now() + CLOSE_TIMEOUT;
    while parse(&read_screen()).is_some_and(|shown| shown.labels == labels) {
        anyhow::ensure!(
            Instant::now() < deadline,
            "{digit} was pressed for row {} ({}), but the menu is still on screen after {} s; \
             it may still take the choice, so check the pane before answering again",
            target + 1,
            labels[target],
            CLOSE_TIMEOUT.as_secs()
        );
        std::thread::sleep(KEY_PAUSE);
    }
    Ok(())
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
            ["Switch to Opus 4.8", "Edit prompt and retry with Opus 5.5"]
        );
        assert_eq!(menu.title, "Session paused");
        assert_eq!(menu.focused, 0);

        let moved = MENU
            .replace("❯ 1.", "  1.")
            .replace("  2. Edit", "❯ 2. Edit");
        assert_eq!(parse(&lines(&moved)).unwrap().focused, 1);
    }

    /// The menu as a live pane showed it, with Claude Code 2.1.296.
    const LIVE_MENU: &str = "──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Session paused                                                                                                                                                                                                                                                                   
                                                                                                                                                                                                                                                                                  
  Opus 5's safeguards flagged this message. Our intentionally broad safeguards allow us to deliver more capabilities faster, but can sometimes flag legitimate coding and cybersecurity tasks. Send feedback with /feedback or learn more:
  https://support.claude.com/en/articles/16049681                                                                                                                                                                                                                                 
                                                                                                                                                                                                                                                                                  
  Details: `[cyber]`
                                                                                                                                                                                                                                                                                  
  ❯ 1. Switch to Opus 4.8
    2. Edit prompt and retry with Opus 5                                                                                                                                                                                                                                          
                                                                                                                                                                                                                                                                                  
✻ Waiting for API response · will retry in 2m 40s · check your network";

    #[test]
    fn parses_the_menu_as_a_live_pane_shows_it() {
        let menu = parse(&lines(LIVE_MENU)).unwrap();
        assert_eq!(
            menu.labels,
            ["Switch to Opus 4.8", "Edit prompt and retry with Opus 5"]
        );
        assert_eq!(menu.focused, 0);
        assert!(menu
            .body
            .starts_with("Opus 5's safeguards flagged this message."));
        assert!(menu.body.ends_with("Details: `[cyber]`"));
    }

    /// The confirmation `/model` raises in a session with history, as a
    /// pane showed it.
    const SWITCH_MODEL: &str = "\
\u{25cf} earlier reply
\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}\u{2594}
   Switch model?
   Your next response will be slower and use more tokens
   This conversation is cached for the current model. Switching to Opus 5.5 means the full history gets re-read on your next message.
   \u{276f} 1. Yes, switch to Opus 5.5
     2. No, go back
";

    #[test]
    fn parses_a_menu_a_slash_command_raises() {
        let menu = parse(&lines(SWITCH_MODEL)).unwrap();
        assert_eq!(menu.title, "Switch model?");
        assert!(menu.body.starts_with("Your next response will be slower"));
        assert!(menu.body.ends_with("re-read on your next message."));
        assert_eq!(menu.labels, ["Yes, switch to Opus 5.5", "No, go back"]);
        assert_eq!(menu.focused, 0);
    }

    #[test]
    fn parses_a_menu_of_more_than_two_choices_with_the_cursor_on_any() {
        let three = SWITCH_MODEL.replace("\u{276f} 1.", "  1.").replace(
            "     2. No, go back",
            "     2. No, go back\n   \u{276f} 3. Ask later",
        );
        let menu = parse(&lines(&three)).unwrap();
        assert_eq!(menu.labels.len(), 3);
        assert_eq!(menu.focused, 2);
    }

    #[test]
    fn other_screens_are_not_a_menu() {
        // Only one choice, no cursor, or two cursors is not a menu to answer.
        let one = MENU.replace(
            "\u{2502}   2. Edit prompt and retry with Opus 5.5     \u{2502}\n",
            "",
        );
        assert_eq!(parse(&lines(&one)), None);
        assert_eq!(parse(&lines(&MENU.replace('\u{276f}', " "))), None);
        let two_cursors = SWITCH_MODEL.replace("     2.", "   \u{276f} 2.");
        assert_eq!(parse(&lines(&two_cursors)), None);
        // Choices that do not count from 1, or have no title under a rule.
        assert_eq!(parse(&lines(&SWITCH_MODEL.replace("2. No", "3. No"))), None);
        let from_two = SWITCH_MODEL
            .replace("1. Yes", "2. Yes")
            .replace("2. No", "3. No");
        assert_eq!(parse(&lines(&from_two)), None);
        let no_rule = SWITCH_MODEL.replace('\u{2594}', "x");
        assert_eq!(parse(&lines(&no_rule)), None);
        assert_eq!(parse(&lines("just a prompt\n\u{276f} ")), None);
        // A numbered list the user is typing at the prompt has one line.
        assert_eq!(parse(&lines("\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} 1. first\n\u{2500}\u{2500}\u{2500}\u{2500}")), None);
    }

    #[test]
    fn only_the_fallback_menu_has_the_fallback_kind() {
        let metadata = metadata();
        let at = Utc::now();
        let switch = parse(&lines(SWITCH_MODEL)).unwrap();
        let request = approval_request(&switch, &metadata, "", at).unwrap();
        assert_eq!(request.kind, MENU_KIND);
        assert_eq!(
            request
                .choices
                .iter()
                .map(|choice| (choice.id.as_str(), choice.label.as_str()))
                .collect::<Vec<_>>(),
            [("1", "Yes, switch to Opus 5.5"), ("2", "No, go back")]
        );
        assert!(request
            .prompt
            .unwrap()
            .starts_with("Switch model?: Your next"));
        // The usage-credits dialog shares the title but not the choices.
        let credits = MENU
            .replace("1. Switch to Opus 4.8", "1. Continue on usage credits")
            .replace("2. Edit prompt and retry with Opus 5.5", "2. Switch models");
        let credits = parse(&lines(&credits)).unwrap();
        assert_eq!(
            approval_request(&credits, &metadata, "", at).unwrap().kind,
            MENU_KIND
        );
        // The same menu shown again later is another request.
        let later = at + chrono::Duration::seconds(30);
        assert_ne!(
            approval_request(&switch, &metadata, "", later)
                .unwrap()
                .request_id,
            request.request_id
        );
    }

    fn metadata() -> AgentMetadata {
        AgentMetadata {
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
        }
    }

    #[test]
    fn the_same_menu_is_the_same_request() {
        let metadata = metadata();
        let at = Utc::now();
        let menu = parse(&lines(MENU)).unwrap();
        let request = approval_request(&menu, &metadata, "turn-1", at).unwrap();
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
        let again = approval_request(&menu, &metadata, "turn-1", at).unwrap();
        assert_eq!(again.request_id, request.request_id);
        let next_turn = approval_request(&menu, &metadata, "turn-2", at).unwrap();
        assert_ne!(next_turn.request_id, request.request_id);
    }

    /// A menu that moves its cursor on arrows and closes on Enter, recording
    /// the keys it receives.
    struct FakeMenu {
        focused: std::cell::Cell<Option<usize>>,
        keys: std::cell::RefCell<Vec<KeyCode>>,
        keys_work: bool,
        /// How many screen reads after a key still show the screen from
        /// before it, as a slow pane does.
        redraw_lag: usize,
        shown: std::cell::Cell<Option<usize>>,
        stale_reads: std::cell::Cell<usize>,
    }

    impl FakeMenu {
        fn new(focused: usize, keys_work: bool) -> Self {
            Self::lagging(focused, keys_work, 0)
        }

        fn lagging(focused: usize, keys_work: bool, redraw_lag: usize) -> Self {
            Self {
                focused: std::cell::Cell::new(Some(focused)),
                keys: Default::default(),
                keys_work,
                redraw_lag,
                shown: std::cell::Cell::new(Some(focused)),
                stale_reads: std::cell::Cell::new(0),
            }
        }

        fn screen(&self) -> Vec<String> {
            if self.stale_reads.get() > 0 {
                self.stale_reads.set(self.stale_reads.get() - 1);
            } else {
                self.shown.set(self.focused.get());
            }
            match self.shown.get() {
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
            // A row's number chooses it and closes the menu.
            if self.keys_work && matches!(key, KeyCode::Char('1' | '2')) {
                self.focused.set(None);
            }
            self.stale_reads.set(self.redraw_lag);
            Ok(())
        }
    }

    const LABELS: [&str; 2] = ["Switch to Opus 4.8", "Edit prompt and retry with Opus 5.5"];

    #[test]
    fn answering_presses_the_chosen_rows_number_wherever_the_cursor_is() {
        for (target, digit) in [(0, '1'), (1, '2')] {
            for focused in [0, 1] {
                let menu = FakeMenu::new(focused, true);
                answer(target, &LABELS, || menu.screen(), |key| menu.press(key)).unwrap();
                assert_eq!(*menu.keys.borrow(), [KeyCode::Char(digit)]);
            }
        }
    }

    #[test]
    fn answering_waits_for_a_slow_pane_to_close_the_menu() {
        // The pane takes about a second to show the key's effect.
        let menu = FakeMenu::lagging(0, true, 6);
        answer(1, &LABELS, || menu.screen(), |key| menu.press(key)).unwrap();
        assert_eq!(*menu.keys.borrow(), [KeyCode::Char('2')]);
    }

    #[test]
    fn a_menu_that_stays_open_gets_no_second_key_and_is_reported() {
        let menu = FakeMenu::new(0, false);
        let error = answer(1, &LABELS, || menu.screen(), |key| menu.press(key)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("2 was pressed for row 2 (Edit prompt and retry with Opus 5.5)"),
            "{}",
            error
        );
        assert_eq!(*menu.keys.borrow(), [KeyCode::Char('2')]);
    }

    #[test]
    fn answering_presses_nothing_for_another_menu_or_none() {
        let menu = FakeMenu::new(0, true);
        let other = ["Switch to Opus 5", "Edit prompt and retry with Opus 5.5"];
        assert!(answer(0, &other, || menu.screen(), |key| menu.press(key)).is_err());
        assert!(menu.keys.borrow().is_empty());
        let gone = FakeMenu::new(0, true);
        gone.focused.set(None);
        assert!(answer(0, &LABELS, || gone.screen(), |key| gone.press(key)).is_err());
        assert!(gone.keys.borrow().is_empty());
    }
}
