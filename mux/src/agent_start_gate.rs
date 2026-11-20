//! Starts the agents a mux restores at startup a few at a time.
//!
//! Every restored pane opens at once, so tabs and layouts appear at once,
//! but an agent's command first waits for a gate file of its own. Started
//! together, about 85 agents each took a median 25 s and up to 66 s to draw
//! a first screen, competing for memory and CPU; started a few at a time,
//! those a user is likely to look at first come up in seconds. Gates open
//! in priority order: agents whose turn a restart interrupted, then agents
//! in tabs that were showing, then agents in parked tabs. A started agent
//! frees its place when a restored pane draws a first screen, or after
//! `SLOT_TIMEOUT` if none does.

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

struct Gates {
    dir: PathBuf,
    /// The rank of each saved pane: lower starts first.
    ranks: HashMap<PaneId, (u8, usize)>,
    /// Gates not yet opened, with their rank.
    waiting: Vec<((u8, usize), PathBuf)>,
    /// When each opened gate's agent started, until a screen is drawn.
    starting: Vec<Instant>,
    /// Whether the restore has registered every gate.
    sealed: bool,
}

static GATES: Mutex<Option<Gates>> = Mutex::new(None);

/// Starts gating, before a restore. `ranks` orders the saved panes whose
/// agents will be restored.
pub fn begin(ranks: HashMap<PaneId, (u8, usize)>) {
    let dir = config::RUNTIME_DIR.join(format!("agent-start-{}", std::process::id()));
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
        waiting: Vec::new(),
        starting: Vec::new(),
        sealed: false,
    });
}

/// Makes the restored agent command of saved pane `saved_pane` wait for its
/// gate before it starts. Unchanged when gating is off.
pub fn gate(saved_pane: PaneId, command: CommandBuilder) -> CommandBuilder {
    let mut gates = GATES.lock();
    let Some(gates) = gates.as_mut() else {
        return command;
    };
    let rank = gates
        .ranks
        .get(&saved_pane)
        .copied()
        .unwrap_or((u8::MAX, usize::MAX));
    let path = gates.dir.join(format!("pane-{saved_pane}"));
    let Some(quoted) = path.to_str().map(shell_words::quote) else {
        return command;
    };
    gates.waiting.push((rank, path.clone()));
    let script =
        format!("while [ ! -e {quoted} ]; do sleep 0.2; done; rm -f {quoted}; exec \"$@\"");
    let mut argv: Vec<OsString> = vec!["sh".into(), "-c".into(), script.into(), "sh".into()];
    argv.extend(command.get_argv().iter().cloned());
    let mut gated = command;
    *gated.get_argv_mut() = argv;
    gated
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
        gates.waiting.sort_by(|a, b| b.0.cmp(&a.0));
        log::info!(
            "startup: starting {} restored agents {CONCURRENT_STARTS} at a time",
            gates.waiting.len()
        );
    }
    open_gates();
    std::thread::Builder::new()
        .name("agent-start-gates".to_string())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            if !open_gates() {
                return;
            }
        })
        .ok();
}

/// Frees a place when a restored pane draws its first screen.
pub fn screen_drawn() {
    if let Some(gates) = GATES.lock().as_mut() {
        if !gates.starting.is_empty() {
            gates.starting.remove(0);
        }
    }
    open_gates();
}

/// Opens gates while fewer than `CONCURRENT_STARTS` agents are starting.
/// Returns false once every gate is open.
fn open_gates() -> bool {
    let mut guard = GATES.lock();
    let Some(gates) = guard.as_mut() else {
        return false;
    };
    gates
        .starting
        .retain(|started| started.elapsed() < SLOT_TIMEOUT);
    while gates.starting.len() < CONCURRENT_STARTS {
        let Some((_, path)) = gates.waiting.pop() else {
            break;
        };
        if let Err(err) = std::fs::write(&path, b"") {
            log::warn!("opening {}: {err:#}", path.display());
        }
        gates.starting.push(Instant::now());
    }
    if gates.sealed && gates.waiting.is_empty() {
        log::info!("startup: every restored agent has been started");
        // The gate files are removed by the commands that wait for them.
        *guard = None;
        return false;
    }
    true
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
        begin(ranks);
        let dir = GATES.lock().as_ref().unwrap().dir.clone();
        for pane in 1..=10 {
            let gated = gate(pane, CommandBuilder::new("claude"));
            let argv = gated.get_argv();
            assert_eq!(argv[0], "sh");
            assert_eq!(argv.last().unwrap(), "claude");
        }
        let opened = |pane: usize| dir.join(format!("pane-{pane}")).exists();

        seal();
        assert!(opened(10));
        assert!((1..=7).all(opened));
        assert!(!opened(8) && !opened(9));
        // A drawn screen frees a place for the next agent in rank order.
        screen_drawn();
        assert!(opened(8) && !opened(9));
        screen_drawn();
        assert!(opened(9));
        assert!(GATES.lock().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
