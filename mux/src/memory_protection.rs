//! Keeps the mux server's own memory out of swap.
//!
//! The server and every program it starts share the systemd service's
//! cgroup, so under memory pressure the kernel pages out the server's
//! scrollback as readily as an idle agent's heap, and a resize or a client
//! attach then waits on swap. On Linux, when the service delegates its
//! cgroup, the server moves itself into a child cgroup that may not swap,
//! and every program it starts afterwards joins a sibling cgroup that may.
//!
//! ```text
//! wakterm-mux-server.service/   memory controller enabled for its children
//!     wakterm-server/           the server; memory.swap.max = 0
//!     wakterm-panes/            panes, helpers, and frozen agents' cgroups
//! ```

use std::path::PathBuf;
use std::sync::OnceLock;

static PANES_CGROUP: OnceLock<PathBuf> = OnceLock::new();

/// The cgroup that programs the server starts join, once `protect_server`
/// has set it up.
pub fn panes_cgroup() -> Option<&'static PathBuf> {
    PANES_CGROUP.get()
}

/// Moves the calling process, which must be the mux server, into a cgroup
/// that may not swap, and makes the programs it starts join one that may.
/// Does nothing unless the server runs in a cgroup delegated to its own
/// user. Call it once, before any pane starts.
#[cfg(target_os = "linux")]
pub fn protect_server() -> anyhow::Result<()> {
    imp::protect_server()
}

#[cfg(not(target_os = "linux"))]
pub fn protect_server() -> anyhow::Result<()> {
    Ok(())
}

/// Makes `command` join the panes' cgroup when it starts.
pub fn spawn_into_panes(command: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    if panes_cgroup().is_some() {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                portable_pty::unix::join_child_cgroup();
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(target_os = "linux")]
mod imp {
    use super::PANES_CGROUP;
    use anyhow::Context;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    pub(super) const SERVER: &str = "wakterm-server";
    pub(super) const PANES: &str = "wakterm-panes";

    fn own_cgroup() -> anyhow::Result<PathBuf> {
        let cgroups = fs::read_to_string("/proc/self/cgroup")?;
        let path = cgroups
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("the mux is not in a cgroup v2 hierarchy")?;
        Ok(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
    }

    pub fn protect_server() -> anyhow::Result<()> {
        // Only a systemd service whose cgroup was delegated to this user
        // may be rearranged; any other cgroup belongs to someone else.
        if std::env::var_os("INVOCATION_ID").is_none() {
            return Ok(());
        }
        let unit = own_cgroup()?;
        let owner = fs::metadata(unit.join("cgroup.subtree_control"))?.uid();
        if owner != unsafe { libc::getuid() } {
            return Ok(());
        }
        let server = unit.join(SERVER);
        let panes = unit.join(PANES);
        for dir in [&server, &panes] {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        move_processes(&unit, &server, &panes)?;
        fs::write(unit.join("cgroup.subtree_control"), "+memory")
            .with_context(|| format!("enabling memory control under {}", unit.display()))?;
        fs::write(server.join("memory.swap.max"), "0")
            .with_context(|| format!("keeping {} out of swap", server.display()))?;
        portable_pty::unix::set_child_cgroup(&panes.join("cgroup.procs"));
        let _ = PANES_CGROUP.set(panes);
        log::info!(
            "the mux server runs in {} without swap; programs it starts join {}",
            server.display(),
            PANES
        );
        Ok(())
    }

    /// Moves this process into `server` and every other process in `unit`
    /// into `panes`, since a cgroup whose children have controllers holds
    /// no processes of its own.
    fn move_processes(unit: &Path, server: &Path, panes: &Path) -> anyhow::Result<()> {
        let own = std::process::id().to_string();
        fs::write(server.join("cgroup.procs"), &own)
            .with_context(|| format!("moving the mux server into {}", server.display()))?;
        for pid in fs::read_to_string(unit.join("cgroup.procs"))?.lines() {
            // A process that exited meanwhile is gone from the cgroup.
            let _ = fs::write(panes.join("cgroup.procs"), pid);
        }
        Ok(())
    }
}
