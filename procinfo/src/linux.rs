#![cfg(target_os = "linux")]
use super::*;
use libc::pid_t;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct LinuxStat {
    pid: pid_t,
    name: String,
    status: String,
    ppid: pid_t,
    // Time process started after boot, measured in ticks.
    starttime: u64,
}

struct LinuxProcessSnapshot {
    processes: HashMap<pid_t, LinuxStat>,
    children: HashMap<pid_t, Vec<pid_t>>,
    roots: Mutex<HashMap<pid_t, Option<LocalProcessInfo>>>,
}

struct CachedLinuxProcessSnapshot {
    captured_at: Instant,
    snapshot: Arc<LinuxProcessSnapshot>,
}

static PROCESS_SNAPSHOT: LazyLock<Mutex<Option<CachedLinuxProcessSnapshot>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
static SNAPSHOT_CAPTURE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

impl LinuxProcessSnapshot {
    fn capture() -> Self {
        #[cfg(test)]
        SNAPSHOT_CAPTURE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut processes = HashMap::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<pid_t>().ok())
                else {
                    continue;
                };
                let Some(process) = Self::read_stat(pid) else {
                    continue;
                };
                processes.insert(pid, process);
            }
        }

        let mut children = HashMap::<pid_t, Vec<pid_t>>::new();
        for process in processes.values() {
            children.entry(process.ppid).or_default().push(process.pid);
        }
        for child_pids in children.values_mut() {
            child_pids.sort_unstable();
        }

        Self {
            processes,
            children,
            roots: Mutex::new(HashMap::new()),
        }
    }

    fn read_stat(pid: pid_t) -> Option<LinuxStat> {
        let data = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_pid_space, name) = data.split_once('(')?;
        let (name, fields) = name.rsplit_once(')')?;
        let fields = fields.split_whitespace().collect::<Vec<_>>();

        Some(LinuxStat {
            pid,
            name: name.to_string(),
            status: fields.first()?.to_string(),
            ppid: fields.get(1)?.parse().ok()?,
            starttime: fields.get(19)?.parse().ok()?,
        })
    }

    fn process(&self, pid: u32) -> Option<LocalProcessInfo> {
        let pid = pid as pid_t;
        if let Some(process) = self.roots.lock().ok()?.get(&pid).cloned() {
            return process;
        }

        let process = self.processes.get(&pid).map(|_| {
            let mut visited = HashSet::new();
            visited.insert(pid as u32);
            self.build_process(pid, &mut visited)
        });
        self.roots.lock().ok()?.insert(pid, process.clone());
        process
    }

    fn build_process(&self, pid: pid_t, visited: &mut HashSet<u32>) -> LocalProcessInfo {
        let info = &self.processes[&pid];
        let mut children = HashMap::new();
        for child_pid in self.children.get(&pid).into_iter().flatten() {
            if visited.insert(*child_pid as u32) {
                children.insert(*child_pid as u32, self.build_process(*child_pid, visited));
            }
        }

        LocalProcessInfo {
            pid: info.pid as u32,
            ppid: info.ppid as u32,
            name: info.name.clone(),
            executable: std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_default(),
            cwd: LocalProcessInfo::current_working_dir(pid as u32).unwrap_or_default(),
            argv: std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|data| {
                    data.strip_suffix(&[0])
                        .unwrap_or(&data)
                        .split(|byte| *byte == 0)
                        .map(|arg| String::from_utf8_lossy(arg).into_owned())
                        .collect()
                })
                .unwrap_or_default(),
            start_time: info.starttime,
            status: info.status.as_str().into(),
            children,
        }
    }
}

fn cached_process_snapshot(max_age: Duration) -> Option<Arc<LinuxProcessSnapshot>> {
    let mut cached = PROCESS_SNAPSHOT.lock().ok()?;
    let expired = max_age.is_zero()
        || cached
            .as_ref()
            .map(|cached| cached.captured_at.elapsed() > max_age)
            .unwrap_or(true);
    if expired {
        *cached = Some(CachedLinuxProcessSnapshot {
            captured_at: Instant::now(),
            snapshot: Arc::new(LinuxProcessSnapshot::capture()),
        });
    }
    cached.as_ref().map(|cached| Arc::clone(&cached.snapshot))
}

impl From<&str> for LocalProcessStatus {
    fn from(s: &str) -> Self {
        match s {
            "R" => Self::Run,
            "S" => Self::Sleep,
            "D" => Self::Idle,
            "Z" => Self::Zombie,
            "T" => Self::Stop,
            "t" => Self::Tracing,
            "X" | "x" => Self::Dead,
            "K" => Self::Wakekill,
            "W" => Self::Waking,
            "P" => Self::Parked,
            _ => Self::Unknown,
        }
    }
}

impl LocalProcessInfo {
    pub fn resident_set_bytes(pid: u32) -> Option<u64> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page_size > 0).then_some(resident_pages.saturating_mul(page_size as u64))
    }

    pub fn current_working_dir(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
    }

    pub fn executable_path(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{}/exe", pid)).ok()
    }

    pub fn with_root_pid(pid: u32) -> Option<Self> {
        cached_process_snapshot(Duration::ZERO)?.process(pid)
    }

    pub fn with_root_pid_cached(pid: u32, max_age: Duration) -> Option<Self> {
        cached_process_snapshot(max_age)?.process(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn cached_process_lookups_share_one_proc_snapshot() {
        *PROCESS_SNAPSHOT.lock().unwrap() = None;
        SNAPSHOT_CAPTURE_COUNT.store(0, Ordering::Relaxed);
        let pid = std::process::id();

        let first = LocalProcessInfo::with_root_pid_cached(pid, Duration::from_secs(1)).unwrap();
        let second = LocalProcessInfo::with_root_pid_cached(pid, Duration::from_secs(1)).unwrap();

        assert_eq!(first.pid, pid);
        assert_eq!(second.pid, pid);
        assert_eq!(SNAPSHOT_CAPTURE_COUNT.load(Ordering::Relaxed), 1);

        LocalProcessInfo::with_root_pid_cached(pid, Duration::from_secs(1)).unwrap();
        LocalProcessInfo::with_root_pid_cached(pid, Duration::ZERO).unwrap();

        assert_eq!(SNAPSHOT_CAPTURE_COUNT.load(Ordering::Relaxed), 2);
    }
}
