//! Asks the agents whose turns a mux restart interrupted to continue them,
//! once each is restored and waiting for input.

use crate::sessionhandler::admit_agent_prompt;
use mux::agent_admission::{AgentAdmissionStatus, AgentPromptAdmissionRequest};
use mux::agent_event::AgentEventKind;
use mux::Mux;
use promise::spawn::spawn_into_main_thread;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const RESUME_PROMPT: &str = "Wakterm's mux server restarted, which interrupted your turn. \
                             Continue where you left off.";
/// How long a restored agent has to become ready for the prompt.
const RESUME_DEADLINE: Duration = Duration::from_secs(600);
/// How long to wait between attempts, and before the first, so that a
/// resumed harness has drawn its input box.
const RESUME_RETRY: Duration = Duration::from_secs(10);

/// Sends each agent, by agent ID and name, the prompt to continue its
/// interrupted turn, retrying while it is not ready.
pub fn start(agents: Vec<(String, String)>) {
    if agents.is_empty() {
        return;
    }
    log::info!(
        "resuming interrupted turns of {}",
        agents
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let since = Mux::get().agent_service().event_store().latest_sequence();
    for (agent_id, name) in agents {
        spawn_into_main_thread(async move { resume(agent_id, name, since).await }).detach();
    }
}

/// Whether the agent took input recorded after event `*cursor`, which then
/// moves past the events read. Input that reached the agent since its
/// restore, such as another message, starts the turn that its interrupted
/// one would have been, so prompting it to continue would be out of place.
async fn took_input_since(agent_id: &str, cursor: &mut u64) -> anyhow::Result<bool> {
    let store = Mux::get().agent_service().event_store();
    loop {
        let page = store.read_page_async(*cursor, 500).await?;
        let took_input = page
            .events
            .iter()
            .any(|event| event.agent_id == agent_id && event.kind == AgentEventKind::InputAccepted);
        if let Some(last) = page.events.last() {
            *cursor = last.sequence;
        }
        if took_input {
            return Ok(true);
        }
        if page.events.is_empty() || page.next_after_sequence.is_none() {
            return Ok(false);
        }
    }
}

async fn resume(agent_id: String, name: String, mut cursor: u64) {
    let deadline = Instant::now() + RESUME_DEADLINE;
    let mut last = String::new();
    while Instant::now() < deadline {
        smol::Timer::after(RESUME_RETRY).await;
        match took_input_since(&agent_id, &mut cursor).await {
            Ok(true) => {
                log::info!(
                    "did not resume the interrupted turn of {name}: it took other input since \
                     its restore"
                );
                return;
            }
            Ok(false) => {}
            Err(err) => {
                last = format!("could not read the agent's events: {err:#}");
                continue;
            }
        }
        let incarnation = Mux::get()
            .agent_service()
            .catalog()
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == agent_id && agent.alive)
            .and_then(|agent| agent.incarnation_id);
        let Some(incarnation_id) = incarnation else {
            last = "the agent is not restored yet".to_string();
            continue;
        };
        // One request per incarnation, so that a retry after an unclear
        // answer cannot write the prompt twice.
        let request_id = format!(
            "{:x}",
            Sha256::digest(format!(
                "resume-interrupted-turn\0{agent_id}\0{incarnation_id}"
            ))
        )[..32]
            .to_string();
        let receipt = match admit_agent_prompt(AgentPromptAdmissionRequest {
            request_id,
            agent_id: agent_id.clone(),
            incarnation_id,
            prompt: RESUME_PROMPT.to_string(),
            paste: false,
            return_final: false,
            timeout_ms: 0,
        })
        .await
        {
            Ok(receipt) => receipt,
            Err(err) => {
                last = format!("{err:#}");
                continue;
            }
        };
        match receipt.status {
            AgentAdmissionStatus::Accepted => {
                log::info!("resumed the interrupted turn of {name}");
                return;
            }
            _ if receipt.prompt_written == Some(true) => {
                log::warn!(
                    "sent {name} the prompt to resume its interrupted turn, but its delivery is \
                     unclear: {}",
                    receipt.detail.unwrap_or_default()
                );
                return;
            }
            AgentAdmissionStatus::Invalid | AgentAdmissionStatus::Unsupported => {
                log::warn!(
                    "could not resume the interrupted turn of {name}: {}",
                    receipt.detail.unwrap_or_default()
                );
                return;
            }
            status => last = receipt.detail.unwrap_or_else(|| format!("{status:?}")),
        }
    }
    log::warn!(
        "gave up resuming the interrupted turn of {name} after {} s: {last}",
        RESUME_DEADLINE.as_secs()
    );
}
