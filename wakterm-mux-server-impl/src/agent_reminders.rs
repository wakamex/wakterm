//! Delivers due agent reminders by typing them into the agent's pane.

use crate::sessionhandler::wait_for_redraw;
use mux::agent::AgentHarness;
use mux::agent_reminder::AgentReminder;
use mux::Mux;
use promise::spawn::spawn_into_main_thread;
use std::time::Duration;

/// How often the mux looks for due reminders.
const REMINDER_TICK: Duration = Duration::from_secs(5);

/// Starts delivering due reminders for as long as the mux runs.
pub fn start() {
    spawn_into_main_thread(async move {
        loop {
            smol::Timer::after(REMINDER_TICK).await;
            if let Err(err) = deliver_due_reminders().await {
                log::warn!("delivering agent reminders failed: {err:#}");
            }
        }
    })
    .detach();
}

async fn deliver_due_reminders() -> anyhow::Result<()> {
    let store = Mux::get().agent_service().reminder_store();
    let now = chrono::Utc::now();
    for reminder in store.due(now)? {
        match deliver(&reminder).await {
            // The agent's pane belongs to another mux on this host, such as
            // a GUI's own mux, which shares this store and delivers it.
            Ok(false) => {}
            Ok(true) => match reminder.after_delivery(now) {
                Some(next) => store.save(&next)?,
                None => {
                    store.cancel(&reminder.id)?;
                }
            },
            // An agent that is busy with a dialog, or not running, gets the
            // reminder on a later attempt.
            Err(err) => {
                let error = format!("{err:#}");
                if reminder.last_error.as_deref() != Some(error.as_str()) {
                    log::info!("agent reminder {} not delivered yet: {error}", reminder.id);
                    store.save(&AgentReminder {
                        last_error: Some(error),
                        ..reminder.clone()
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Types the reminder into its agent's pane and presses Enter once the pane
/// has drawn it, as `wakterm agent send` does. Returns false when the agent
/// has no pane in this mux.
async fn deliver(reminder: &AgentReminder) -> anyhow::Result<bool> {
    let mux = Mux::get();
    let Some(agent) = mux
        .agent_service()
        .list_agents_cached()
        .into_iter()
        .find(|agent| agent.metadata.agent_id == reminder.agent_id)
    else {
        return Ok(false);
    };
    if !agent.runtime.alive {
        anyhow::bail!("the agent is not running");
    }
    if let Some(reason) = mux::agent::input_blocked_reason(&agent.runtime) {
        anyhow::bail!("{reason}");
    }
    let pane = mux
        .get_pane(agent.pane_id)
        .ok_or_else(|| anyhow::anyhow!("the agent's pane is gone"))?;
    // A GUI mirrors a mux server's panes; the server delivers to those.
    if pane.downcast_ref::<mux::localpane::LocalPane>().is_none() {
        return Ok(false);
    }
    let paste = !matches!(agent.runtime.harness, AgentHarness::Gemini);
    pane.send_prompt(&reminder.message, paste)?;
    wait_for_redraw(agent.pane_id).await;
    pane.writer().write_all(b"\r")?;
    Ok(true)
}
