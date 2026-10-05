//! Answers a pending Claude Code AskUserQuestion dialog by pressing its
//! default keys in the agent's pane, checking the screen after each step.
//!
//! The dialog shows one question at a time. A digit picks a single-select
//! option and moves on, or toggles a multi-select option; the digit after the
//! options focuses the "Type something." box, whose text Enter records. Tab
//! moves to the next question, and after the last one a review lists every
//! answered question as its text followed by a `→ <answer>` line, then asks
//! "Ready to submit your answers?", which Enter confirms. A form of one
//! single-select question has no review: picking its answer submits it.
//! "Chat about this" sits below the last list item, and Escape cancels.

use crate::agent_approval::{AgentApprovalAnswer, AgentApprovalQuestion};
use crate::pane::Pane;
use anyhow::{bail, Context};
use std::sync::Arc;
use std::time::{Duration, Instant};
use wakterm_term::{KeyCode, KeyModifiers};

/// How long to wait for the dialog to show the state a step leads to.
const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Pause between keys that do not change the visible question.
const KEY_PAUSE: Duration = Duration::from_millis(120);
/// Characters of a question or answer that identify it on screen. Long text
/// wraps, so only its start is matched.
const MATCH_PREFIX: usize = 30;

const REVIEW_TITLE: &str = "Review your answers";
const REVIEW_PROMPT: &str = "Ready to submit your answers?";
const UNANSWERED_WARNING: &str = "You have not answered all questions";
const CHAT_LABEL: &str = "Chat about this";
const POINTER: char = '❯';
const ANSWER_ARROW: char = '→';

/// What the user decided for the whole form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FormAction {
    Submit(Vec<AgentApprovalAnswer>),
    Chat,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Key(KeyCode),
    Paste(String),
    /// The dialog shows this question.
    AwaitQuestion(usize),
    /// The typed text is visible.
    AwaitText(String),
    /// The review lists exactly the intended answers.
    AwaitReview,
    /// The pointer is on "Chat about this".
    AwaitChatFocus,
}

/// Checks the answers against the questions and returns the keys and screen
/// checks that enter them, starting from the first question.
pub(crate) fn plan(
    questions: &[AgentApprovalQuestion],
    action: &FormAction,
) -> anyhow::Result<Vec<Step>> {
    anyhow::ensure!(!questions.is_empty(), "the question form has no questions");
    let mut steps = vec![Step::AwaitQuestion(0)];
    let answers = match action {
        FormAction::Cancel => {
            steps.push(Step::Key(KeyCode::Escape));
            return Ok(steps);
        }
        FormAction::Chat => {
            // The options, then "Type something.", then "Chat about this".
            for _ in 0..=questions[0].options.len() {
                steps.push(Step::Key(KeyCode::DownArrow));
            }
            steps.extend([Step::AwaitChatFocus, Step::Key(KeyCode::Enter)]);
            return Ok(steps);
        }
        FormAction::Submit(answers) => answers,
    };

    let mut by_question = vec![None; questions.len()];
    for answer in answers {
        let index = answer.question as usize;
        let question = questions
            .get(index)
            .with_context(|| format!("the form has no question {index}"))?;
        anyhow::ensure!(
            by_question[index].replace(answer).is_none(),
            "question {index} is answered twice"
        );
        for choice in &answer.choices {
            anyhow::ensure!(
                question.options.iter().any(|option| &option.id == choice),
                "question {index} has no choice {choice}"
            );
        }
        let text = answer
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if question.multi_select {
            anyhow::ensure!(
                text.is_none(),
                "question {index} allows several choices; a typed answer is only supported for single-choice questions"
            );
            anyhow::ensure!(
                !answer.choices.is_empty(),
                "question {index} has an answer with no choices"
            );
        } else {
            anyhow::ensure!(
                answer.choices.len() + usize::from(text.is_some()) == 1,
                "question {index} takes exactly one choice or a typed answer"
            );
        }
    }
    let reviewed = questions.len() > 1 || questions[0].multi_select;
    anyhow::ensure!(
        reviewed || by_question[0].is_some(),
        "a form with one single-choice question needs its answer"
    );

    for (index, question) in questions.iter().enumerate() {
        let last = index + 1 == questions.len();
        match by_question[index] {
            None => steps.push(Step::Key(KeyCode::Tab)),
            Some(answer) => {
                let digit = |position: usize| {
                    char::from_digit(position as u32, 10)
                        .map(KeyCode::Char)
                        .context("the question has too many options for digit keys")
                };
                let text = answer
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                if let Some(text) = text {
                    steps.push(Step::Key(digit(question.options.len() + 1)?));
                    steps.push(Step::Paste(text.to_string()));
                    steps.push(Step::AwaitText(text.to_string()));
                    steps.push(Step::Key(KeyCode::Enter));
                } else {
                    for choice in &answer.choices {
                        let position = question
                            .options
                            .iter()
                            .position(|option| &option.id == choice)
                            .expect("choices were checked against the options");
                        steps.push(Step::Key(digit(position + 1)?));
                    }
                    // A multi-select records each toggle; Tab moves on.
                    if question.multi_select {
                        steps.push(Step::Key(KeyCode::Tab));
                    }
                }
            }
        }
        if !last {
            steps.push(Step::AwaitQuestion(index + 1));
        }
    }
    if reviewed {
        steps.extend([Step::AwaitReview, Step::Key(KeyCode::Enter)]);
    }
    Ok(steps)
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prefix(text: &str) -> String {
    let start: String = normalize(text).chars().take(MATCH_PREFIX).collect();
    start.trim_end().to_string()
}

fn screen_has(lines: &[String], text: &str) -> bool {
    let wanted = prefix(text);
    lines.iter().any(|line| normalize(line).contains(&wanted))
}

/// Whether the dialog is open on this question.
pub(crate) fn shows_question(lines: &[String], question: &AgentApprovalQuestion) -> bool {
    screen_has(lines, &question.question) && lines.iter().any(|line| line.contains(CHAT_LABEL))
}

pub(crate) fn chat_focused(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.contains(CHAT_LABEL) && line.contains(POINTER))
}

/// Checks that the review lists exactly the intended answers, in question
/// order, and warns about unanswered questions exactly when some are.
pub(crate) fn check_review(
    lines: &[String],
    questions: &[AgentApprovalQuestion],
    answers: &[AgentApprovalAnswer],
) -> anyhow::Result<()> {
    let start = lines
        .iter()
        .rposition(|line| line.contains(REVIEW_TITLE))
        .context("the review is not on screen")?;
    let end = lines[start..]
        .iter()
        .position(|line| line.contains(REVIEW_PROMPT))
        .map(|offset| start + offset)
        .context("the review's submit prompt is not on screen")?;
    let review = &lines[start..end];

    // Each answer is a `→` line, continued by wrapped lines up to the next
    // question's text or answer.
    let mut shown = vec![];
    for line in review {
        let trimmed = line.trim_start();
        if let Some(answer) = trimmed.strip_prefix(ANSWER_ARROW) {
            shown.push(normalize(answer));
        } else if let Some(last) = shown.last_mut() {
            if !trimmed.is_empty() {
                last.push(' ');
                last.push_str(&normalize(trimmed));
            }
        }
    }

    let mut answered = answers.to_vec();
    answered.sort_by_key(|answer| answer.question);
    anyhow::ensure!(
        shown.len() == answered.len(),
        "the review lists {} answers instead of {}",
        shown.len(),
        answered.len()
    );
    for (answer, shown) in answered.iter().zip(&shown) {
        let question = &questions[answer.question as usize];
        let expected = match answer
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(text) => vec![prefix(text)],
            None => answer
                .choices
                .iter()
                .filter_map(|choice| question.options.iter().find(|option| &option.id == choice))
                .map(|option| normalize(&option.label))
                .collect(),
        };
        for expected in expected {
            anyhow::ensure!(
                shown.contains(&expected),
                "the review shows {shown:?} for question {}, without {expected:?}",
                answer.question
            );
        }
    }
    let warns = review.iter().any(|line| line.contains(UNANSWERED_WARNING));
    anyhow::ensure!(
        warns == (answered.len() < questions.len()),
        "the review's unanswered-question warning does not match the answers"
    );
    Ok(())
}

fn screen(pane: &Arc<dyn Pane>) -> Vec<String> {
    let dims = pane.get_dimensions();
    let top = dims.physical_top;
    let (_, lines) = pane.get_lines(top..top + dims.viewport_rows as isize);
    lines
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect()
}

fn await_screen(
    pane: &Arc<dyn Pane>,
    what: &str,
    ready: impl Fn(&[String]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let lines = screen(pane);
        match ready(&lines) {
            Ok(()) => return Ok(()),
            Err(err) if Instant::now() >= deadline => {
                bail!("stopped before submitting: {what}: {err:#}")
            }
            Err(_) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Presses the planned keys, stopping without submitting at the first
/// screen that does not show what the plan expects.
pub(crate) fn run(
    pane: &Arc<dyn Pane>,
    questions: &[AgentApprovalQuestion],
    action: &FormAction,
    steps: &[Step],
) -> anyhow::Result<()> {
    let answers = match action {
        FormAction::Submit(answers) => answers.as_slice(),
        _ => &[],
    };
    for step in steps {
        match step {
            Step::Key(key) => {
                pane.key_down(key.clone(), KeyModifiers::NONE)?;
                std::thread::sleep(KEY_PAUSE);
            }
            Step::Paste(text) => {
                pane.send_paste(text)?;
                std::thread::sleep(KEY_PAUSE);
            }
            Step::AwaitQuestion(index) => {
                let question = &questions[*index];
                await_screen(
                    pane,
                    "the dialog does not show the expected question",
                    |lines| {
                        anyhow::ensure!(
                            shows_question(lines, question),
                            "question {index} {:?} is not on screen",
                            prefix(&question.question)
                        );
                        Ok(())
                    },
                )?
            }
            Step::AwaitText(text) => {
                await_screen(pane, "the typed answer did not appear", |lines| {
                    anyhow::ensure!(
                        screen_has(lines, text),
                        "{:?} is not on screen",
                        prefix(text)
                    );
                    Ok(())
                })?
            }
            Step::AwaitReview => await_screen(pane, "the review does not match", |lines| {
                check_review(lines, questions, answers)
            })?,
            Step::AwaitChatFocus => {
                await_screen(pane, "\"Chat about this\" is not selected", |lines| {
                    anyhow::ensure!(chat_focused(lines), "the pointer is elsewhere");
                    Ok(())
                })?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::agent_approval::AgentApprovalChoice;

    fn question(index: u32, text: &str, multi_select: bool) -> AgentApprovalQuestion {
        AgentApprovalQuestion {
            index,
            header: Some(format!("Q{index}")),
            question: text.to_string(),
            multi_select,
            options: ["Alpha", "Beta", "Gamma"]
                .iter()
                .enumerate()
                .map(|(i, label)| AgentApprovalChoice {
                    id: format!("option_{}", i + 1),
                    label: label.to_string(),
                    description: None,
                })
                .collect(),
        }
    }

    fn answer(question: u32, choices: &[&str], text: Option<&str>) -> AgentApprovalAnswer {
        AgentApprovalAnswer {
            question,
            choices: choices.iter().map(|c| c.to_string()).collect(),
            text: text.map(str::to_string),
        }
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn plans_digits_text_and_tabs_then_checks_the_review() {
        let questions = [
            question(0, "Which browser?", false),
            question(1, "Which tools?", true),
            question(2, "Anything else?", false),
            question(3, "Which lifetime?", false),
        ];
        let action = FormAction::Submit(vec![
            answer(0, &["option_2"], None),
            answer(1, &["option_1", "option_3"], None),
            answer(3, &[], Some("ten days")),
        ]);
        use KeyCode::*;
        assert_eq!(
            plan(&questions, &action).unwrap(),
            vec![
                Step::AwaitQuestion(0),
                Step::Key(Char('2')),
                Step::AwaitQuestion(1),
                Step::Key(Char('1')),
                Step::Key(Char('3')),
                Step::Key(Tab),
                Step::AwaitQuestion(2),
                Step::Key(Tab),
                Step::AwaitQuestion(3),
                Step::Key(Char('4')),
                Step::Paste("ten days".to_string()),
                Step::AwaitText("ten days".to_string()),
                Step::Key(Enter),
                Step::AwaitReview,
                Step::Key(Enter),
            ]
        );
    }

    #[test]
    fn a_single_choice_question_submits_without_a_review() {
        let questions = [question(0, "Which browser?", false)];
        let steps = plan(
            &questions,
            &FormAction::Submit(vec![answer(0, &["option_3"], None)]),
        )
        .unwrap();
        assert_eq!(
            steps,
            vec![Step::AwaitQuestion(0), Step::Key(KeyCode::Char('3'))]
        );
        assert!(plan(&questions, &FormAction::Submit(vec![])).is_err());
    }

    #[test]
    fn rejects_answers_the_dialog_cannot_take() {
        let questions = [
            question(0, "Which browser?", false),
            question(1, "Which tools?", true),
        ];
        for answers in [
            vec![answer(2, &["option_1"], None)],
            vec![answer(0, &["option_9"], None)],
            vec![answer(0, &["option_1", "option_2"], None)],
            vec![answer(0, &["option_1"], Some("both"))],
            vec![answer(1, &["option_1"], Some("more"))],
            vec![
                answer(0, &["option_1"], None),
                answer(0, &["option_2"], None),
            ],
        ] {
            assert!(
                plan(&questions, &FormAction::Submit(answers.clone())).is_err(),
                "{:?}",
                answers
            );
        }
    }

    #[test]
    fn chat_moves_below_the_list_and_cancel_escapes() {
        let questions = [question(0, "Which browser?", false)];
        use KeyCode::*;
        assert_eq!(
            plan(&questions, &FormAction::Chat).unwrap(),
            vec![
                Step::AwaitQuestion(0),
                Step::Key(DownArrow),
                Step::Key(DownArrow),
                Step::Key(DownArrow),
                Step::Key(DownArrow),
                Step::AwaitChatFocus,
                Step::Key(Enter),
            ]
        );
        assert_eq!(
            plan(&questions, &FormAction::Cancel).unwrap(),
            vec![Step::AwaitQuestion(0), Step::Key(Escape)]
        );
    }

    #[test]
    fn the_review_must_show_every_intended_answer() {
        let questions = [
            question(
                0,
                "Which browser should the tool use for signing in?",
                false,
            ),
            question(1, "Which tools?", true),
            question(2, "Anything else?", false),
        ];
        let answers = [
            answer(0, &["option_2"], None),
            answer(1, &["option_1", "option_3"], None),
        ];
        let screen = lines(
            "● Asking a question\n\
             ☐ Q0  ☐ Q1  ☐ Q2  ✔ Submit\n\
             \n\
             Review your answers\n\
             \n\
             ⚠ You have not answered all questions\n\
             \x20● Which browser should the tool use for signing in?\n\
             \x20  → Beta\n\
             \x20● Which tools?\n\
             \x20  → Alpha, Gamma\n\
             \n\
             Ready to submit your answers?\n\
             ❯ 1. Submit answers\n\
             \x20 2. Cancel",
        );
        check_review(&screen, &questions, &answers).unwrap();

        // A wrong pick, a missing answer, or a wrong warning stops the submit.
        let wrong = screen.join("\n").replace("→ Beta", "→ Alpha");
        assert!(check_review(&lines(&wrong), &questions, &answers).is_err());
        let missing = screen.join("\n").replace("→ Alpha, Gamma", "→ Alpha");
        assert!(check_review(&lines(&missing), &questions, &answers).is_err());
        let all = [
            answers[0].clone(),
            answers[1].clone(),
            answer(2, &["option_1"], None),
        ];
        assert!(check_review(&screen, &questions, &all).is_err());
    }

    #[test]
    fn recognizes_the_open_dialog_and_chat_focus() {
        let q = question(
            0,
            "Which way should the tool get exact prices automatically?",
            false,
        );
        let screen = lines(
            "Which way should the tool get exact\n\
             prices automatically?\n\
             ❯ 1. Alpha\n\
             \x20 2. Beta\n\
             \x20 3. Gamma\n\
             \x20 4. Type something.\n\
             \x20 5. Chat about this",
        );
        assert!(shows_question(&screen, &q));
        assert!(!chat_focused(&screen));
        let focused = screen
            .join("\n")
            .replace("❯ 1. Alpha", "  1. Alpha")
            .replace("  5. Chat", "❯ 5. Chat");
        assert!(chat_focused(&lines(&focused)));
    }
}
