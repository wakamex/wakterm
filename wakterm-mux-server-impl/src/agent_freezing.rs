//! Freezes idle agents periodically, when `agent_idle_freeze` is on.

use mux::Mux;
use promise::spawn::spawn_into_main_thread;
use std::time::Duration;

/// How often the mux looks for idle agents to freeze.
const FREEZE_TICK: Duration = Duration::from_secs(5);

/// Starts freezing idle agents for as long as the mux runs.
pub fn start() {
    spawn_into_main_thread(async move {
        loop {
            smol::Timer::after(FREEZE_TICK).await;
            Mux::get().freeze_idle_agents();
        }
    })
    .detach();
}
