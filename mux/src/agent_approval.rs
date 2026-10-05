use crate::agent::AgentMetadata;
use crate::agent_admission::incarnation_id;
use crate::Mux;
use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub const AGENT_APPROVAL_SCHEMA: &str = "wakterm.agent-approval.v1";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentApprovalChoice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentApprovalRequest {
    pub schema: String,
    #[serde(default = "command_approval_kind")]
    pub kind: String,
    pub request_id: String,
    pub agent_id: String,
    pub incarnation_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub prompt: Option<String>,
    pub reason: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub choices: Vec<AgentApprovalChoice>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentApprovalResolutionRequest {
    pub request_id: String,
    pub agent_id: String,
    pub incarnation_id: String,
    pub choice_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentApprovalResolution {
    pub schema: String,
    pub request_id: String,
    pub agent_id: String,
    pub incarnation_id: String,
    pub choice_id: String,
    pub resolved: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PendingAgentApproval {
    pub request: AgentApprovalRequest,
    pub(crate) app_server_request_id: Value,
    thread_id: String,
    responses: BTreeMap<String, Value>,
}

impl Mux {
    pub(crate) fn capture_agent_approval(&self, message: &Value) -> Option<AgentApprovalRequest> {
        if !matches!(
            message.get("method").and_then(Value::as_str),
            Some("item/commandExecution/requestApproval" | "item/tool/requestUserInput")
        ) {
            return None;
        }
        let params = message.get("params")?;
        let thread_id = params.get("threadId")?.as_str()?;
        let metadata = self
            .agent_metadata_by_pane
            .read()
            .values()
            .find_map(|metadata| {
                (metadata
                    .codex_app_server
                    .as_ref()
                    .is_some_and(|session| session.thread_id == thread_id))
                .then(|| metadata.clone())
            })?;
        let pending = parse_codex_approval(message, &metadata)?;
        let request = pending.request.clone();
        self.pending_agent_approvals
            .write()
            .insert(request.request_id.clone(), pending);
        Some(request)
    }

    pub fn resolve_agent_approval(
        &self,
        resolution: AgentApprovalResolutionRequest,
    ) -> anyhow::Result<AgentApprovalResolution> {
        let Some(pending) = self
            .pending_agent_approvals
            .write()
            .remove(&resolution.request_id)
        else {
            return self.resolve_claude_question(resolution);
        };
        let result = (|| {
            anyhow::ensure!(
                pending.request.agent_id == resolution.agent_id
                    && pending.request.incarnation_id == resolution.incarnation_id,
                "approval target identity changed"
            );
            let current = self
                .agent_metadata_by_pane
                .read()
                .values()
                .find(|metadata| metadata.agent_id == resolution.agent_id)
                .cloned()
                .context("approval target is no longer available")?;
            anyhow::ensure!(
                incarnation_id(&current).as_deref() == Some(resolution.incarnation_id.as_str()),
                "approval target incarnation changed"
            );
            let response = pending
                .responses
                .get(&resolution.choice_id)
                .cloned()
                .context("approval choice is not available")?;
            self.codex_app_server
                .respond(pending.app_server_request_id.clone(), response)?;
            Ok(AgentApprovalResolution {
                schema: AGENT_APPROVAL_SCHEMA.to_string(),
                request_id: resolution.request_id.clone(),
                agent_id: resolution.agent_id.clone(),
                incarnation_id: resolution.incarnation_id.clone(),
                choice_id: resolution.choice_id.clone(),
                resolved: true,
            })
        })();
        if result.is_err() {
            self.pending_agent_approvals
                .write()
                .insert(resolution.request_id, pending);
        }
        result
    }

    fn resolve_claude_question(
        &self,
        resolution: AgentApprovalResolutionRequest,
    ) -> anyhow::Result<AgentApprovalResolution> {
        let request = self
            .agent_event_store
            .find_approval(&resolution.request_id)?
            .context("approval request is no longer pending")?;
        anyhow::ensure!(
            request.kind != "user_question_form",
            "this question form has no choices and must be answered in the agent's pane"
        );
        anyhow::ensure!(
            request.kind == "user_question"
                && request.agent_id == resolution.agent_id
                && request.incarnation_id == resolution.incarnation_id,
            "approval target identity changed"
        );
        let choice = request
            .choices
            .iter()
            .find(|choice| choice.id == resolution.choice_id)
            .context("approval choice is not available")?;
        let (pane_id, metadata) = self
            .agent_metadata_by_pane
            .read()
            .iter()
            .find(|(_, metadata)| metadata.agent_id == resolution.agent_id)
            .map(|(pane_id, metadata)| (*pane_id, metadata.clone()))
            .context("approval target is no longer available")?;
        anyhow::ensure!(
            incarnation_id(&metadata).as_deref() == Some(resolution.incarnation_id.as_str()),
            "approval target incarnation changed"
        );
        let runtime = self
            .agent_runtime_by_pane
            .read()
            .get(&pane_id)
            .cloned()
            .context("approval target runtime is unavailable")?;
        anyhow::ensure!(
            runtime.alive && runtime.harness == crate::agent::AgentHarness::Claude,
            "approval target is not a live Claude session"
        );
        let session_path = runtime
            .session_path
            .as_deref()
            .context("approval target has no exact Claude session")?;
        anyhow::ensure!(
            claude_question_is_pending(Path::new(session_path), &request.item_id)?,
            "approval request is no longer pending"
        );
        let pane = self
            .get_pane(pane_id)
            .context("approval target pane disappeared")?;
        anyhow::ensure!(!pane.is_dead(), "approval target pane exited");
        pane.send_text_and_submit(&choice.label, true)?;
        self.record_agent_prompt_submission(pane_id);
        Ok(AgentApprovalResolution {
            schema: AGENT_APPROVAL_SCHEMA.to_string(),
            request_id: resolution.request_id,
            agent_id: resolution.agent_id,
            incarnation_id: resolution.incarnation_id,
            choice_id: resolution.choice_id,
            resolved: true,
        })
    }

    pub(crate) fn expire_agent_approval(&self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").unwrap_or(&Value::Null);
        match method {
            Some("serverRequest/resolved") => {
                let Some(request_id) = params.get("requestId") else {
                    return;
                };
                let thread_id = params.get("threadId").and_then(Value::as_str);
                self.pending_agent_approvals.write().retain(|_, pending| {
                    pending.app_server_request_id != *request_id
                        || thread_id.is_some_and(|thread_id| pending.thread_id != thread_id)
                });
            }
            Some("turn/completed") => {
                let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
                    return;
                };
                let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) else {
                    return;
                };
                self.pending_agent_approvals.write().retain(|_, pending| {
                    pending.thread_id != thread_id || pending.request.turn_id != turn_id
                });
            }
            _ => {}
        }
    }
}

fn claude_question_is_pending(path: &Path, item_id: &str) -> anyhow::Result<bool> {
    let reader = BufReader::new(File::open(path)?);
    let mut found = false;
    for line in reader.lines() {
        let Ok(record) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if !found {
            found = record
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|content| {
                    content.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_use")
                            && block.get("name").and_then(Value::as_str) == Some("AskUserQuestion")
                            && block.get("id").and_then(Value::as_str) == Some(item_id)
                    })
                });
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => return Ok(false),
            Some("user") => {
                let content = record.pointer("/message/content");
                let matching_result = content.and_then(Value::as_array).is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                            && block.get("tool_use_id").and_then(Value::as_str) == Some(item_id)
                    })
                });
                let human_message = !content.and_then(Value::as_array).is_some_and(|blocks| {
                    !blocks.is_empty()
                        && blocks.iter().all(|block| {
                            block.get("type").and_then(Value::as_str) == Some("tool_result")
                        })
                });
                if matching_result || human_message {
                    return Ok(false);
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

pub(crate) fn approval_from_event(
    message: &Value,
    metadata: &AgentMetadata,
) -> Option<AgentApprovalRequest> {
    parse_codex_approval(message, metadata).map(|pending| pending.request)
}

/// The approval request for a Claude AskUserQuestion block. A single
/// single-choice question is a `user_question` whose choices answer it. Any
/// other form, with several questions or multi-select answers, is a
/// `user_question_form` that lists every question in its prompt and has no
/// choices, because it can only be answered in the agent's pane.
pub(crate) fn claude_question_from_block(
    block: &Value,
    metadata: &AgentMetadata,
    turn_id: &str,
    observed_at: DateTime<Utc>,
) -> Option<AgentApprovalRequest> {
    let item_id = block.get("id")?.as_str()?.to_string();
    let questions = block.pointer("/input/questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let incarnation = incarnation_id(metadata)?;
    let request_key = format!(
        "claude\0{}\0{incarnation}\0{turn_id}\0{item_id}",
        metadata.agent_id
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let request = |kind: &str, prompt, reason, choices| AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: kind.to_string(),
        request_id: request_id.clone(),
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation.clone(),
        turn_id: turn_id.to_string(),
        item_id: item_id.clone(),
        observed_at,
        prompt: Some(prompt),
        reason,
        command: None,
        cwd: None,
        choices,
    };
    if let [question] = questions.as_slice() {
        if let Some(choices) = claude_single_choices(question) {
            return Some(request(
                "user_question",
                question.get("question")?.as_str()?.to_string(),
                question
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                choices,
            ));
        }
    }
    Some(request(
        "user_question_form",
        claude_question_form_text(questions)?,
        None,
        vec![],
    ))
}

/// The choices of a single-choice question with at least two options.
fn claude_single_choices(question: &Value) -> Option<Vec<AgentApprovalChoice>> {
    if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let options = question.get("options")?.as_array()?;
    if options.len() < 2 {
        return None;
    }
    options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            Some(AgentApprovalChoice {
                id: format!("option_{}", index + 1),
                label: option.get("label")?.as_str()?.to_string(),
                description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Every question of a form with its options, one question per paragraph.
fn claude_question_form_text(questions: &[Value]) -> Option<String> {
    let mut paragraphs = vec![];
    for (index, question) in questions.iter().enumerate() {
        let mut text = format!("{}. ", index + 1);
        if let Some(header) = question.get("header").and_then(Value::as_str) {
            text.push_str(&format!("{header}: "));
        }
        text.push_str(question.get("question")?.as_str()?);
        if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
            text.push_str(" (choose any)");
        }
        for option in question
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            text.push_str("\n- ");
            text.push_str(option.get("label")?.as_str()?);
            if let Some(description) = option.get("description").and_then(Value::as_str) {
                text.push_str(&format!(": {description}"));
            }
        }
        paragraphs.push(text);
    }
    Some(paragraphs.join("\n\n"))
}

fn command_approval_kind() -> String {
    "command_approval".to_string()
}

fn parse_codex_approval(message: &Value, metadata: &AgentMetadata) -> Option<PendingAgentApproval> {
    match message.get("method").and_then(Value::as_str)? {
        "item/commandExecution/requestApproval" => parse_command_approval(message, metadata),
        "item/tool/requestUserInput" => parse_codex_question(message, metadata),
        _ => None,
    }
}

fn parse_command_approval(
    message: &Value,
    metadata: &AgentMetadata,
) -> Option<PendingAgentApproval> {
    let app_server_request_id = message.get("id")?.clone();
    let params = message.get("params")?;
    let thread_id = params.get("threadId")?.as_str()?;
    if !metadata
        .codex_app_server
        .as_ref()
        .is_some_and(|session| session.thread_id == thread_id)
    {
        return None;
    }
    let incarnation = incarnation_id(metadata)?;
    let turn_id = params.get("turnId")?.as_str()?.to_string();
    let item_id = params.get("itemId")?.as_str()?.to_string();
    let request_key = format!(
        "{thread_id}\0{turn_id}\0{item_id}\0{}",
        scalar_request_id(&app_server_request_id)?
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let decisions = effective_decisions(params);
    let mut responses = BTreeMap::new();
    let mut choices = Vec::new();
    for decision in decisions {
        let Some((choice_id, label)) = describe_decision(&decision) else {
            continue;
        };
        if responses.contains_key(choice_id) {
            continue;
        }
        responses.insert(choice_id.to_string(), json!({"decision": decision}));
        choices.push(AgentApprovalChoice {
            id: choice_id.to_string(),
            label,
            description: None,
        });
    }
    if choices.is_empty() {
        return None;
    }
    let request = AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: command_approval_kind(),
        request_id: request_id.clone(),
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation,
        turn_id,
        item_id,
        observed_at: Utc::now(),
        prompt: None,
        reason: params
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        command: params
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        cwd: params
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
        choices,
    };
    Some(PendingAgentApproval {
        request,
        app_server_request_id,
        thread_id: thread_id.to_string(),
        responses,
    })
}

fn parse_codex_question(message: &Value, metadata: &AgentMetadata) -> Option<PendingAgentApproval> {
    let app_server_request_id = message.get("id")?.clone();
    let params = message.get("params")?;
    let thread_id = params.get("threadId")?.as_str()?;
    if !metadata
        .codex_app_server
        .as_ref()
        .is_some_and(|session| session.thread_id == thread_id)
        || params.get("isBlocking").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    let questions = params.get("questions")?.as_array()?;
    let [question] = questions.as_slice() else {
        return None;
    };
    let options = question.get("options")?.as_array()?;
    if options.len() < 2 {
        return None;
    }
    let question_id = question.get("id")?.as_str()?;
    let turn_id = params.get("turnId")?.as_str()?.to_string();
    let item_id = params.get("itemId")?.as_str()?.to_string();
    let request_key = format!(
        "{thread_id}\0{turn_id}\0{item_id}\0{}",
        scalar_request_id(&app_server_request_id)?
    );
    let request_id = format!("{:x}", Sha256::digest(request_key.as_bytes()))[..24].to_string();
    let mut choices = Vec::with_capacity(options.len());
    let mut responses = BTreeMap::new();
    for (index, option) in options.iter().enumerate() {
        let label = option.get("label")?.as_str()?.to_string();
        let choice_id = format!("option_{}", index + 1);
        responses.insert(
            choice_id.clone(),
            json!({"answers": {question_id: {"answers": [label]}}}),
        );
        choices.push(AgentApprovalChoice {
            id: choice_id,
            label,
            description: option
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let request = AgentApprovalRequest {
        schema: AGENT_APPROVAL_SCHEMA.to_string(),
        kind: "user_question".to_string(),
        request_id,
        agent_id: metadata.agent_id.clone(),
        incarnation_id: incarnation_id(metadata)?,
        turn_id,
        item_id,
        observed_at: Utc::now(),
        prompt: Some(question.get("question")?.as_str()?.to_string()),
        reason: question
            .get("header")
            .and_then(Value::as_str)
            .map(str::to_string),
        command: None,
        cwd: None,
        choices,
    };
    Some(PendingAgentApproval {
        request,
        app_server_request_id,
        thread_id: thread_id.to_string(),
        responses,
    })
}

fn scalar_request_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn effective_decisions(params: &Value) -> Vec<Value> {
    if let Some(decisions) = params.get("availableDecisions").and_then(Value::as_array) {
        return decisions.clone();
    }
    let mut decisions = vec![Value::String("accept".into())];
    if let Some(amendment) = params
        .get("proposedExecpolicyAmendment")
        .filter(|value| !value.is_null())
    {
        decisions.push(json!({
            "acceptWithExecpolicyAmendment": {"execpolicy_amendment": amendment}
        }));
    } else if params
        .get("additionalPermissions")
        .is_none_or(Value::is_null)
    {
        if params
            .get("networkApprovalContext")
            .is_some_and(|value| !value.is_null())
        {
            decisions.push(Value::String("acceptForSession".into()));
        }
    }
    decisions.push(Value::String("cancel".into()));
    decisions
}

fn describe_decision(decision: &Value) -> Option<(&'static str, String)> {
    match decision.as_str() {
        Some("accept") => Some(("allow_once", "Allow once".into())),
        Some("acceptForSession") => Some(("allow_session", "Allow for this session".into())),
        Some("decline") => Some(("reject", "Reject".into())),
        Some("cancel") => Some(("reject", "Reject".into())),
        _ => {
            let object = decision.as_object()?;
            if let Some(amendment) = object.get("acceptWithExecpolicyAmendment") {
                let prefix = amendment
                    .get("execpolicy_amendment")
                    .map(render_command_prefix)
                    .unwrap_or_else(|| "matching commands".into());
                return Some(("allow_prefix", format!("Always allow: {prefix}")));
            }
            if object.contains_key("applyNetworkPolicyAmendment") {
                return Some(("allow_network", "Always allow this network access".into()));
            }
            None
        }
    }
}

fn render_command_prefix(value: &Value) -> String {
    let rendered = match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let mut rendered = rendered.replace(['\n', '\r'], " ");
    if rendered.chars().count() > 48 {
        rendered = rendered.chars().take(45).collect::<String>() + "...";
    }
    if rendered.is_empty() {
        "matching commands".into()
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::CodexAppServerSession;
    use std::io::Write as _;

    fn metadata() -> AgentMetadata {
        AgentMetadata {
            agent_id: "agent-w3sim".into(),
            name: "w3sim_codex".into(),
            launch_cmd: "codex".into(),
            declared_cwd: "/code/w3sim".into(),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: Some(CodexAppServerSession {
                thread_id: "thread-1".into(),
                session_id: "session-1".into(),
                executable: "codex".into(),
                version: "test".into(),
                tui_args: vec![],
            }),
            launch_supervisor: None,
        }
    }

    #[test]
    fn command_approval_preserves_exact_native_decisions() {
        let message = json!({
            "id": 17,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": "scp result host:/tmp/",
                "cwd": "/code/w3sim",
                "proposedExecpolicyAmendment": ["scp", "result"],
                "availableDecisions": [
                    "accept",
                    {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["scp", "result"]}},
                    "cancel"
                ]
            }
        });
        let pending = parse_command_approval(&message, &metadata()).unwrap();
        assert_eq!(
            pending.request.choices,
            vec![
                AgentApprovalChoice {
                    id: "allow_once".into(),
                    label: "Allow once".into(),
                    description: None,
                },
                AgentApprovalChoice {
                    id: "allow_prefix".into(),
                    label: "Always allow: scp result".into(),
                    description: None,
                },
                AgentApprovalChoice {
                    id: "reject".into(),
                    label: "Reject".into(),
                    description: None,
                },
            ]
        );
        assert_eq!(
            pending.responses["allow_prefix"],
            json!({"decision": {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["scp", "result"]}}})
        );
        assert_eq!(pending.app_server_request_id, json!(17));
        assert_eq!(pending.thread_id, "thread-1");
    }

    #[test]
    fn command_approval_rejects_another_thread() {
        assert!(parse_command_approval(
            &json!({
                "id": 18,
                "method": "item/commandExecution/requestApproval",
                "params": {"threadId": "thread-2", "turnId": "turn-1", "itemId": "item-1"}
            }),
            &metadata()
        )
        .is_none());
    }

    #[test]
    fn codex_single_choice_question_preserves_native_answer() {
        let message = json!({
            "id": "request-7",
            "method": "item/tool/requestUserInput",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-2",
                "itemId": "item-question",
                "isBlocking": true,
                "questions": [{
                    "id": "promotion",
                    "header": "Promotion scope",
                    "question": "How should I promote the build?",
                    "options": [
                        {"label": "Build and activate", "description": "Replace the shared runtime."},
                        {"label": "Hold off", "description": "Keep the current runtime."}
                    ]
                }]
            }
        });
        let pending = parse_codex_question(&message, &metadata()).unwrap();
        assert_eq!(pending.request.kind, "user_question");
        assert_eq!(
            pending.request.prompt.as_deref(),
            Some("How should I promote the build?")
        );
        assert_eq!(pending.request.choices[1].id, "option_2");
        assert_eq!(
            pending.responses["option_2"],
            json!({"answers": {"promotion": {"answers": ["Hold off"]}}})
        );
    }

    #[test]
    fn claude_question_forms_list_every_question_without_choices() {
        let metadata = metadata();
        let question = |header: &str, multi: bool| {
            json!({"question": format!("Which {header}?"), "header": header, "multiSelect": multi,
                "options": [{"label": "A", "description": "first"}, {"label": "B"}]})
        };
        let block = |questions: Vec<Value>| {
            json!({"type": "tool_use", "id": "toolu_form", "name": "AskUserQuestion",
                "input": {"questions": questions}})
        };
        let form = |questions| {
            claude_question_from_block(&block(questions), &metadata, "turn", Utc::now()).unwrap()
        };

        let single = form(vec![question("Browser", false)]);
        assert_eq!(single.kind, "user_question");
        assert_eq!(single.choices.len(), 2);

        // Several questions, or one with multi-select answers, cannot be
        // answered with one choice.
        let several = form(vec![
            question("Browser", false),
            question("Lifetime", false),
        ]);
        assert_eq!(several.kind, "user_question_form");
        assert!(several.choices.is_empty());
        assert_eq!(
            several.prompt.as_deref(),
            Some(
                "1. Browser: Which Browser?\n- A: first\n- B\n\n\
                 2. Lifetime: Which Lifetime?\n- A: first\n- B"
            )
        );
        assert_eq!(several.request_id, single.request_id);
        let multi = form(vec![question("Tools", true)]);
        assert_eq!(multi.kind, "user_question_form");
        assert_eq!(
            multi.prompt.as_deref(),
            Some("1. Tools: Which Tools? (choose any)\n- A: first\n- B")
        );
    }

    #[test]
    fn claude_question_is_exact_and_becomes_stale_after_an_answer() {
        let block = json!({
            "type": "tool_use",
            "id": "tool-question",
            "name": "AskUserQuestion",
            "input": {"questions": [{
                "header": "Promotion scope",
                "question": "How should I promote the build?",
                "multiSelect": false,
                "options": [
                    {"label": "Build and activate", "description": "Replace the shared runtime."},
                    {"label": "Hold off", "description": "Keep the current runtime."}
                ]
            }]}
        });
        let mut claude = metadata();
        claude.codex_app_server = None;
        claude.adopted_pid = Some(12);
        claude.adopted_start_time = Some(34);
        let request = claude_question_from_block(&block, &claude, "turn-3", Utc::now()).unwrap();
        assert_eq!(request.kind, "user_question");
        assert_eq!(request.choices[0].label, "Build and activate");

        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("claude.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type": "assistant", "message": {"content": [block]}})
            ),
        )
        .unwrap();
        assert!(claude_question_is_pending(&path, "tool-question").unwrap());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                format!(
                    "{}\n",
                    json!({"type": "user", "message": {"content": [{
                        "type": "tool_result",
                        "tool_use_id": "tool-question",
                        "content": "answered"
                    }]}})
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(!claude_question_is_pending(&path, "tool-question").unwrap());
    }

    #[test]
    fn provider_resolution_expires_the_exact_pending_request() {
        let mux = Mux::new(None);
        mux.agent_metadata_by_pane
            .write()
            .insert(7, std::sync::Arc::new(metadata()));
        let message = json!({
            "id": 17,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "command": "true",
                "availableDecisions": ["accept", "cancel"]
            }
        });
        let request = mux.capture_agent_approval(&message).unwrap();
        assert!(mux
            .pending_agent_approvals
            .read()
            .contains_key(&request.request_id));
        mux.expire_agent_approval(&json!({
            "method": "serverRequest/resolved",
            "params": {"threadId": "thread-1", "requestId": 17}
        }));
        assert!(mux.pending_agent_approvals.read().is_empty());
    }
}
