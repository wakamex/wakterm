//! Starts the agents a mux restores at startup a few at a time.
//!
//! Every restored pane opens at once, so tabs and layouts appear at once,
//! but an agent's command first waits for a gate file of its own. Started
//! together, about 85 agents each took a median 25 s and up to 66 s to draw
//! a first screen, competing for memory and CPU; started a few at a time,
//! those a user is likely to look at first come up in seconds. Gates open
//! in priority order: agents whose turn a restart interrupted, then agents
//! in tabs that were showing, then agents in parked tabs. A started agent
//! frees its place when its pane draws a first screen, or after
//! `SLOT_TIMEOUT` if it does not.

use crate::pane::PaneId;
use parking_lot::Mutex;
use portable_pty::CommandBuilder;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Agents allowed to be starting at once.
const CONCURRENT_STARTS: usize = 8;
/// A started agent that has drawn nothing by then frees its place anyway.
const SLOT_TIMEOUT: Duration = Duration::from_secs(30);

/// A gated agent, keyed by its saved pane.
struct Agent {
    name: String,
    rank: (u8, usize),
    path: PathBuf,
    /// When its gate opened.
    started: Option<Instant>,
    /// How long after starting it drew a first screen, or None once it used
    /// its whole place without one.
    drew: Option<Option<Duration>>,
}

struct Gates {
    dir: PathBuf,
    ranks: HashMap<PaneId, (u8, usize)>,
    agents: HashMap<PaneId, Agent>,
    /// The saved pane each restored pane was restored from.
    restored_from: HashMap<PaneId, PaneId>,
    /// Whether the restore has gated every agent it will start.
    sealed: bool,
}

impl Gates {
    fn starting(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| agent.started.is_some() && agent.drew.is_none())
            .count()
    }
}

static GATES: Mutex<Option<Gates>> = Mutex::new(None);

/// Starts gating, before a restore. `ranks` orders the saved panes whose
/// agents will be restored: lower starts first.
pub fn begin(ranks: HashMap<PaneId, (u8, usize)>) {
    begin_in(
        config::RUNTIME_DIR.join(format!("agent-start-{}", std::process::id())),
        ranks,
    );
}

/// Starts gating with the gate files in `dir`.
fn begin_in(dir: PathBuf, ranks: HashMap<PaneId, (u8, usize)>) {
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log::warn!(
            "agents start without gates: creating {}: {err:#}",
            dir.display()
        );
        return;
    }
    *GATES.lock() = Some(Gates {
        dir,
        ranks,
        agents: HashMap::new(),
        restored_from: HashMap::new(),
        sealed: false,
    });
}

/// Makes the restored command of agent `name`, saved in pane `saved_pane`,
/// wait for its gate before it starts. Unchanged when gating is off.
pub fn gate(saved_pane: PaneId, name: &str, command: CommandBuilder) -> CommandBuilder {
    let mut gates = GATES.lock();
    let Some(gates) = gates.as_mut() else {
        return command;
    };
    let path = gates.dir.join(format!("pane-{saved_pane}"));
    let Some(quoted) = path
        .to_str()
        .map(|path| shell_words::quote(path).into_owned())
    else {
        return command;
    };
    let rank = gates
        .ranks
        .get(&saved_pane)
        .copied()
        .unwrap_or((u8::MAX, usize::MAX));
    gates.agents.insert(
        saved_pane,
        Agent {
            name: name.to_string(),
            rank,
            path,
            started: None,
            drew: None,
        },
    );
    let script =
        format!("while [ ! -e {quoted} ]; do sleep 0.2; done; rm -f {quoted}; exec \"$@\"");
    let mut argv: Vec<OsString> = vec!["sh".into(), "-c".into(), script.into(), "sh".into()];
    argv.extend(command.get_argv().iter().cloned());
    let mut gated = command;
    *gated.get_argv_mut() = argv;
    gated
}

/// Notes that the agent saved in `saved_pane` was restored into `pane_id`.
pub fn bind(saved_pane: PaneId, pane_id: PaneId) {
    if let Some(gates) = GATES.lock().as_mut() {
        gates.restored_from.insert(pane_id, saved_pane);
    }
}

/// Notes that the restore has gated every agent it will start, and opens
/// the first gates.
pub fn seal() {
    {
        let mut gates = GATES.lock();
        let Some(gates) = gates.as_mut() else {
            return;
        };
        gates.sealed = true;
        log::info!(
            "startup: starting {} restored agents {CONCURRENT_STARTS} at a time",
            gates.agents.len()
        );
    }
    advance();
    std::thread::Builder::new()
        .name("agent-start-gates".to_string())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            if !advance() {
                return;
            }
        })
        .ok();
}

/// Notes that restored pane `pane_id` drew its first screen, which frees
/// its agent's place.
pub fn screen_drawn(pane_id: PaneId) {
    if let Some(gates) = GATES.lock().as_mut() {
        if let Some(saved) = gates.restored_from.get(&pane_id).copied() {
            if let Some(agent) = gates.agents.get_mut(&saved) {
                if let (Some(started), None) = (agent.started, agent.drew) {
                    agent.drew = Some(Some(started.elapsed()));
                }
            }
        }
    }
    advance();
}

/// Frees the places of agents that used their whole place, opens gates
/// while fewer than `CONCURRENT_STARTS` agents are starting, and reports
/// once every agent has started and drawn or used its place. Returns false
/// when nothing is left to do.
fn advance() -> bool {
    let mut guard = GATES.lock();
    let Some(gates) = guard.as_mut() else {
        return false;
    };
    for agent in gates.agents.values_mut() {
        if agent.drew.is_none() && agent.started.is_some_and(|at| at.elapsed() >= SLOT_TIMEOUT) {
            agent.drew = Some(None);
        }
    }
    let mut waiting = gates
        .agents
        .iter()
        .filter(|(_, agent)| agent.started.is_none())
        .map(|(saved, agent)| (agent.rank, *saved))
        .collect::<Vec<_>>();
    waiting.sort();
    let mut free = CONCURRENT_STARTS.saturating_sub(gates.starting());
    for (_, saved) in waiting {
        if free == 0 || !gates.sealed {
            break;
        }
        let agent = gates.agents.get_mut(&saved).expect("waiting agent");
        if let Err(err) = std::fs::write(&agent.path, b"") {
            log::warn!("opening {}: {err:#}", agent.path.display());
        }
        agent.started = Some(Instant::now());
        free -= 1;
    }
    let done = gates.sealed && gates.agents.values().all(|agent| agent.drew.is_some());
    if done {
        report(gates);
        // The gate files are removed by the commands that wait for them.
        *guard = None;
        return false;
    }
    true
}

fn report(gates: &Gates) {
    let mut drawn = gates
        .agents
        .values()
        .filter_map(|agent| Some((agent.drew.flatten()?, agent.name.as_str())))
        .collect::<Vec<_>>();
    let silent = gates
        .agents
        .values()
        .filter(|agent| agent.drew == Some(None))
        .map(|agent| agent.name.as_str())
        .collect::<Vec<_>>();
    drawn.sort();
    let median = drawn
        .get(drawn.len() / 2)
        .map_or(0.0, |(took, _)| took.as_secs_f64());
    let slowest = drawn
        .iter()
        .rev()
        .take(5)
        .map(|(took, name)| format!("{name} {:.1} s", took.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(", ");
    log::info!(
        "startup: every restored agent has started; {} drew a first screen a median \
         {median:.1} s after starting, slowest {slowest}; {} drew none within {} s: {}",
        drawn.len(),
        silent.len(),
        SLOT_TIMEOUT.as_secs(),
        silent.join(", ")
    );
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn agents_start_by_rank_a_few_at_a_time() {
        // Pane 10's turn was interrupted, panes 1 to 8 were showing, and
        // pane 9 was parked.
        let mut ranks = HashMap::new();
        ranks.insert(10, (0, 0));
        for pane in 1..=8 {
            ranks.insert(pane, (1, pane));
        }
        ranks.insert(9, (2, 9));
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("gates");
        begin_in(dir.clone(), ranks);
        for pane in 1..=10 {
            let gated = gate(pane, &format!("agent{pane}"), CommandBuilder::new("claude"));
            // Restored panes get new IDs.
            bind(pane, pane + 100);
            let argv = gated.get_argv();
            assert_eq!(argv[0], "sh");
            assert_eq!(argv.last().unwrap(), "claude");
        }
        let opened = |pane: usize| dir.join(format!("pane-{pane}")).exists();

        seal();
        assert!(opened(10));
        assert!((1..=7).all(opened));
        assert!(!opened(8) && !opened(9));
        // A pane that is not a gated agent drawing frees nothing.
        screen_drawn(42);
        assert!(!opened(8));
        // An agent's pane drawing frees its place for the next in rank
        // order.
        screen_drawn(110);
        assert!(opened(8) && !opened(9));
        screen_drawn(101);
        assert!(opened(9));
        // Once every agent has drawn, gating ends.
        for pane in [102, 103, 104, 105, 106, 107, 108, 109] {
            screen_drawn(pane);
        }
        assert!(GATES.lock().is_none());
    }
}
