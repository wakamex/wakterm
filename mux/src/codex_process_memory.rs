//! Mux-wide Codex process memory, sampled away from the mux and GUI threads.

use crate::agent::{is_harness_tui_process, AgentHarness};
use crate::Mux;
use chrono::Utc;
use parking_lot::Mutex;
use procinfo::LocalProcessInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexProcessMemory {
    pub sampled_at_ms: u64,
    pub process_count: u32,
    /// None means at least one process could not be measured, or PSS is not
    /// supported on this host. A partial sum must not appear as a complete total.
    pub pss_bytes: Option<u64>,
}

#[derive(Default)]
struct Cache {
    snapshot: Option<CodexProcessMemory>,
    sampled_at: Option<Instant>,
    sampling: bool,
}

#[derive(Default)]
pub(crate) struct CodexProcessMemoryCache {
    state: Arc<Mutex<Cache>>,
}

impl CodexProcessMemoryCache {
    pub fn get(&self, mux: Weak<Mux>) -> Option<CodexProcessMemory> {
        let mut state = self.state.lock();
        let result = state.snapshot.clone();
        if state.sampling
            || state
                .sampled_at
                .is_some_and(|at| at.elapsed() < SAMPLE_INTERVAL)
        {
            return result;
        }
        state.sampling = true;
        let cache = Arc::clone(&self.state);
        smol::spawn(async move {
            smol::unblock(move || {
                let snapshot = mux.upgrade().map(|mux| sample(&mux));
                let mut state = cache.lock();
                state.snapshot = snapshot;
                state.sampled_at = Some(Instant::now());
                state.sampling = false;
            })
            .await;
        })
        .detach();
        result
    }
}

fn sample(mux: &Mux) -> CodexProcessMemory {
    let mut processes = HashMap::new();
    let mut complete = true;
    for agent in mux.list_agents_cached() {
        if agent.runtime.harness != AgentHarness::Codex || !agent.runtime.alive {
            continue;
        }
        match (
            agent.metadata.adopted_pid,
            agent.metadata.adopted_start_time,
        ) {
            (Some(pid), Some(start)) => {
                processes.insert(pid, Some(start));
            }
            _ => complete = false,
        }
    }
    if let Some(pid) = mux.codex_app_server.process_id() {
        processes.entry(pid).or_insert(None);
    }
    sample_processes(processes, complete)
}

fn collect_codex_processes(
    process: &LocalProcessInfo,
    processes: &mut HashMap<u32, Option<u64>>,
) -> bool {
    let mut found = is_harness_tui_process(&AgentHarness::Codex, process);
    if found {
        processes.insert(process.pid, Some(process.start_time));
    }
    for child in process.children.values() {
        found |= collect_codex_processes(child, processes);
    }
    found
}

fn sample_processes(roots: HashMap<u32, Option<u64>>, mut complete: bool) -> CodexProcessMemory {
    let mut processes = HashMap::new();
    for (&pid, &expected_start) in &roots {
        #[cfg(target_os = "linux")]
        let process = LocalProcessInfo::with_root_pid_cached(pid, SAMPLE_INTERVAL);
        #[cfg(not(target_os = "linux"))]
        let process = LocalProcessInfo::with_root_pid(pid);
        let Some(process) = process else {
            complete = false;
            continue;
        };
        if expected_start.is_some_and(|start| process.start_time != start) {
            complete = false;
            continue;
        }
        complete &= collect_codex_processes(&process, &mut processes);
    }
    measure_processes(processes, complete)
}

fn measure_processes(processes: HashMap<u32, Option<u64>>, complete: bool) -> CodexProcessMemory {
    let pss_bytes = processes.iter().try_fold(0u64, |total, (&pid, &start)| {
        total.checked_add(LocalProcessInfo::proportional_set_bytes(pid, start)?)
    });
    CodexProcessMemory {
        sampled_at_ms: Utc::now().timestamp_millis().max(0) as u64,
        process_count: processes.len() as u32,
        pss_bytes: if complete && cfg!(target_os = "linux") {
            pss_bytes
        } else {
            None
        },
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn process_total_uses_kernel_pss_and_rejects_partial_measurements() {
        let pid = std::process::id();
        let process = LocalProcessInfo::with_root_pid(pid).unwrap();
        let processes = HashMap::from([(pid, Some(process.start_time))]);
        let snapshot = measure_processes(processes.clone(), true);
        assert_eq!(snapshot.process_count, 1);
        assert!(snapshot.pss_bytes.unwrap() > 0);
        assert!(snapshot.sampled_at_ms > 0);
        assert_eq!(measure_processes(processes, false).pss_bytes, None);
        let missing = sample_processes(HashMap::from([(pid, Some(process.start_time + 1))]), true);
        assert_eq!(missing.pss_bytes, None);
    }

    #[test]
    fn overlapping_codex_trees_count_each_process_once_and_exclude_tools() {
        let mut process = LocalProcessInfo::with_root_pid(std::process::id()).unwrap();
        process.name = "codex".to_string();
        process.executable = "/usr/bin/codex".into();
        process.argv.clear();
        process.children.clear();
        let mut server = process.clone();
        server.pid += 1;
        let mut tool = process.clone();
        tool.pid += 2;
        tool.name = "python".to_string();
        tool.executable = "/usr/bin/python".into();
        process.children.insert(server.pid, server.clone());
        process.children.insert(tool.pid, tool.clone());

        let mut processes = HashMap::new();
        assert!(collect_codex_processes(&process, &mut processes));
        assert!(collect_codex_processes(&server, &mut processes));
        assert!(!collect_codex_processes(&tool, &mut processes));
        assert_eq!(processes.len(), 2);
        assert!(processes.contains_key(&process.pid));
        assert!(processes.contains_key(&server.pid));
    }
}
