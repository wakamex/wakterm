//! Freezes idle agent processes so the operating system can page out their
//! memory, and thaws them before input reaches them.
//!
//! On Linux an agent is frozen with the cgroup v2 freezer: the process alone
//! moves into a child of the mux's own cgroup, which is frozen. Unlike
//! SIGSTOP, this sends no signal, so the job-control shell the agent runs
//! under does not see it stop and keeps the terminal with it.

/// Freezes the process.
pub fn freeze(pid: u32) -> anyhow::Result<()> {
    imp::freeze(pid)
}

/// Thaws a process frozen by `freeze`. A process that has exited is not an
/// error.
pub fn thaw(pid: u32) -> anyhow::Result<()> {
    imp::thaw(pid)
}

/// Thaws processes that an earlier mux froze and did not thaw, such as
/// after a crash.
pub fn thaw_leftovers() -> anyhow::Result<()> {
    imp::thaw_leftovers()
}

/// Whether this platform can freeze agents.
pub fn supported() -> bool {
    imp::supported()
}

#[cfg(target_os = "linux")]
mod imp {
    use anyhow::Context;
    use std::fs;
    use std::path::PathBuf;

    const PREFIX: &str = "wakterm-frozen-";

    /// The mux's own cgroup directory.
    pub(super) fn own_cgroup() -> anyhow::Result<PathBuf> {
        let cgroups = fs::read_to_string("/proc/self/cgroup")?;
        let path = cgroups
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("the mux is not in a cgroup v2 hierarchy")?;
        Ok(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
    }

    pub fn supported() -> bool {
        own_cgroup().is_ok_and(|dir| dir.join("cgroup.procs").exists())
    }

    pub fn freeze(pid: u32) -> anyhow::Result<()> {
        let dir = own_cgroup()?.join(format!("{PREFIX}{pid}"));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::write(dir.join("cgroup.procs"), pid.to_string())
            .with_context(|| format!("moving process {pid} into {}", dir.display()))?;
        fs::write(dir.join("cgroup.freeze"), "1")
            .with_context(|| format!("freezing {}", dir.display()))?;
        Ok(())
    }

    pub fn thaw(pid: u32) -> anyhow::Result<()> {
        let root = own_cgroup()?;
        thaw_dir(&root, &root.join(format!("{PREFIX}{pid}")))
    }

    /// Thaws a freeze cgroup, moves its processes back to the mux's cgroup
    /// and removes it.
    fn thaw_dir(root: &PathBuf, dir: &PathBuf) -> anyhow::Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        fs::write(dir.join("cgroup.freeze"), "0")
            .with_context(|| format!("thawing {}", dir.display()))?;
        for pid in fs::read_to_string(dir.join("cgroup.procs"))?.lines() {
            // A process that exited meanwhile is gone from the cgroup.
            let _ = fs::write(root.join("cgroup.procs"), pid);
        }
        fs::remove_dir(dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(())
    }

    pub fn thaw_leftovers() -> anyhow::Result<()> {
        let root = own_cgroup()?;
        for entry in fs::read_dir(&root)? {
            let dir = entry?.path();
            if dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(PREFIX))
            {
                thaw_dir(&root, &dir)?;
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn supported() -> bool {
        false
    }

    pub fn freeze(_pid: u32) -> anyhow::Result<()> {
        anyhow::bail!("freezing agents is not supported on this platform")
    }

    pub fn thaw(_pid: u32) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn thaw_leftovers() -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod test {
    use super::*;

    #[test]
    fn frozen_process_stays_in_its_state_and_thaws_back() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        // A test host may not let this process manage its cgroup.
        if !supported() || freeze(pid).is_err() {
            let _ = thaw(pid);
            let _ = child.kill();
            return;
        }
        let dir = imp::own_cgroup()
            .unwrap()
            .join(format!("wakterm-frozen-{pid}"));
        // The kernel freezes the cgroup asynchronously.
        let frozen = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            std::fs::read_to_string(dir.join("cgroup.events"))
                .unwrap()
                .contains("frozen 1")
        });
        assert!(frozen);
        // No stop signal: the parent sees no change.
        assert!(child.try_wait().unwrap().is_none());

        thaw(pid).unwrap();
        assert!(!dir.exists());
        let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        assert!(!cgroup.contains("wakterm-frozen-"));
        // Thawing again, or a process that is not frozen, is harmless.
        thaw(pid).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
