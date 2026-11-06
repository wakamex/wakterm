//! Asks the agents whose turns a mux restart interrupted to continue them,
//! once each is restored and waiting for input.

use crate::sessionhandler::admit_agent_prompt;
use mux::agent_admission::{AgentAdmissionStatus, AgentPromptAdmissionRequest};
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
    for (agent_id, name) in agents {
        spawn_into_main_thread(async move { resume(agent_id, name).await }).detach();
    }
}

async fn resume(agent_id: String, name: String) {
    let deadline = Instant::now() + RESUME_DEADLINE;
    let mut last = String::new();
    while Instant::now() < deadline {
        smol::Timer::after(RESUME_RETRY).await;
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
