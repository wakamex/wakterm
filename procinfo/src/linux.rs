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
    process_group: u32,
    controlling_tty: Option<u64>,
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

/// When set, a capture waits at this barrier as it starts and again before
/// it reads /proc, so a test can act while a capture is in progress.
#[cfg(test)]
static CAPTURE_PAUSE: Mutex<Option<Arc<std::sync::Barrier>>> = Mutex::new(None);

/// Serializes the tests that use the process-wide snapshot.
#[cfg(test)]
static SNAPSHOT_TEST_LOCK: Mutex<()> = Mutex::new(());

impl LinuxProcessSnapshot {
    fn capture() -> Self {
        #[cfg(test)]
        {
            SNAPSHOT_CAPTURE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pause = CAPTURE_PAUSE.lock().unwrap().clone();
            if let Some(pause) = pause {
                pause.wait();
                pause.wait();
            }
        }

        Self::scan()
    }

    /// Reads every process's stat.
    fn scan() -> Self {
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
            process_group: fields.get(2)?.parse().ok()?,
            controlling_tty: fields
                .get(4)?
                .parse::<i64>()
                .ok()
                .filter(|tty| *tty != 0)
                .map(|tty| tty as u64),
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
            process_group: info.process_group,
            controlling_tty: info.controlling_tty,
            name: info.name.clone(),
            executable: std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_default(),
            cwd: LocalProcessInfo::current_working_dir(pid as u32).unwrap_or_default(),
            argv: read_argv(pid),
            start_time: info.starttime,
            status: info.status.as_str().into(),
            children,
        }
    }
}

/// Returns a snapshot no older than `max_age`, capturing a new one when the
/// cached one is older. The capture runs outside the lock, so a slow capture
/// on one thread never holds up a thread that can use the cached snapshot.
fn cached_process_snapshot(max_age: Duration) -> Option<Arc<LinuxProcessSnapshot>> {
    if !max_age.is_zero() {
        let cached = PROCESS_SNAPSHOT.lock().ok()?;
        if let Some(cached) = cached
            .as_ref()
            .filter(|cached| cached.captured_at.elapsed() <= max_age)
        {
            return Some(Arc::clone(&cached.snapshot));
        }
    }

    let captured_at = Instant::now();
    let snapshot = Arc::new(LinuxProcessSnapshot::capture());
    let mut cached = PROCESS_SNAPSHOT.lock().ok()?;
    // A capture that started later on another thread is the newer one.
    if cached
        .as_ref()
        .map_or(true, |cached| cached.captured_at <= captured_at)
    {
        *cached = Some(CachedLinuxProcessSnapshot {
            captured_at,
            snapshot: Arc::clone(&snapshot),
        });
    }
    Some(snapshot)
}

/// How often the sampler reads the process trees that lookups ask for,
/// counted from the end of one pass to the start of the next.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(300);
/// A root nobody has looked up for this long is no longer sampled.
const FORGET_ROOT_AFTER: Duration = Duration::from_secs(30);

/// The process trees of the roots that lookups ask for, read by one
/// background thread and published together, so that a lookup only reads
/// memory. Each pass reads every process's stat, which is cheaper than
/// following the asked-for trees through each thread's `children` list:
/// agents run hundreds of threads. Executables and arguments are read once
/// per process, working directories on each pass.
struct TreeSampler {
    wants: Mutex<SamplerWants>,
    wake: std::sync::Condvar,
    published: std::sync::RwLock<Arc<HashMap<pid_t, Option<LocalProcessInfo>>>>,
}

#[derive(Default)]
struct SamplerWants {
    /// Each root asked for, and when it was last asked for.
    roots: HashMap<pid_t, Instant>,
    /// A root was asked for that the sampler has not read yet.
    new_root: bool,
}

/// A process's executable and arguments, which change only when it execs,
/// which also renames it. Kept for as long as the process is sampled.
#[derive(Clone)]
struct ProcessImage {
    starttime: u64,
    name: String,
    executable: PathBuf,
    argv: Vec<String>,
}

/// When set, the sampler waits at this barrier before it publishes, twice,
/// so a test can look up processes while a pass is in progress.
#[cfg(test)]
static SAMPLE_PAUSE: Mutex<Option<Arc<std::sync::Barrier>>> = Mutex::new(None);

static TREE_SAMPLER: LazyLock<Arc<TreeSampler>> = LazyLock::new(|| {
    let sampler = Arc::new(TreeSampler {
        wants: Mutex::new(SamplerWants::default()),
        wake: std::sync::Condvar::new(),
        published: std::sync::RwLock::new(Arc::new(HashMap::new())),
    });
    let running = Arc::clone(&sampler);
    std::thread::Builder::new()
        .name("process-sampler".to_string())
        .spawn(move || running.run())
        .expect("failed to start the process sampler thread");
    sampler
});

impl TreeSampler {
    fn lookup(&self, pid: pid_t) -> Option<LocalProcessInfo> {
        let published = Arc::clone(
            &self
                .published
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut wants = self
            .wants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if wants.roots.insert(pid, Instant::now()).is_none() {
            wants.new_root = true;
            self.wake.notify_one();
        }
        drop(wants);
        published.get(&pid).cloned().flatten()
    }

    fn run(&self) {
        let mut images = HashMap::new();
        loop {
            let roots = {
                let wants = self
                    .wants
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let (mut wants, _) = self
                    .wake
                    .wait_timeout_while(wants, SAMPLE_INTERVAL, |wants| !wants.new_root)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                wants.new_root = false;
                wants
                    .roots
                    .retain(|_, asked| asked.elapsed() < FORGET_ROOT_AFTER);
                wants.roots.keys().copied().collect::<Vec<_>>()
            };
            // A pass that panics is logged and the thread keeps sampling,
            // so lookups are not left with the last pass for good.
            let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let snapshot = LinuxProcessSnapshot::scan();
                let mut seen = HashMap::new();
                let trees = roots
                    .into_iter()
                    .map(|root| {
                        let mut visited = HashSet::new();
                        let tree = sample_tree(&snapshot, root, &mut visited, &images, &mut seen);
                        (root, tree)
                    })
                    .collect::<HashMap<_, _>>();
                (trees, seen)
            }));
            let trees = match pass {
                Ok((trees, seen)) => {
                    images = seen;
                    trees
                }
                Err(_) => {
                    log::error!("reading process trees panicked; reading them again");
                    images.clear();
                    std::thread::sleep(SAMPLE_INTERVAL);
                    continue;
                }
            };
            #[cfg(test)]
            {
                let pause = SAMPLE_PAUSE.lock().unwrap().clone();
                if let Some(pause) = pause {
                    pause.wait();
                    pause.wait();
                }
            }
            let previous = std::mem::replace(
                &mut *self
                    .published
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                Arc::new(trees),
            );
            drop(previous);
        }
    }
}

/// Builds the tree rooted at `pid` from `snapshot`, reusing the executable
/// and arguments of processes `images` already holds and recording every
/// process in `seen`. None when the process does not exist.
fn sample_tree(
    snapshot: &LinuxProcessSnapshot,
    pid: pid_t,
    visited: &mut HashSet<pid_t>,
    images: &HashMap<pid_t, ProcessImage>,
    seen: &mut HashMap<pid_t, ProcessImage>,
) -> Option<LocalProcessInfo> {
    if !visited.insert(pid) {
        return None;
    }
    let stat = snapshot.processes.get(&pid)?;
    let image = match images.get(&pid) {
        Some(image) if image.starttime == stat.starttime && image.name == stat.name => {
            image.clone()
        }
        _ => ProcessImage {
            starttime: stat.starttime,
            name: stat.name.clone(),
            executable: std::fs::read_link(format!("/proc/{pid}/exe")).unwrap_or_default(),
            argv: read_argv(pid),
        },
    };
    seen.insert(pid, image.clone());
    let children = snapshot
        .children
        .get(&pid)
        .into_iter()
        .flatten()
        .filter_map(|&child| {
            sample_tree(snapshot, child, visited, images, seen)
                .map(|process| (child as u32, process))
        })
        .collect();
    Some(LocalProcessInfo {
        pid: pid as u32,
        ppid: stat.ppid as u32,
        process_group: stat.process_group,
        controlling_tty: stat.controlling_tty,
        name: stat.name.clone(),
        executable: image.executable,
        cwd: LocalProcessInfo::current_working_dir(pid as u32).unwrap_or_default(),
        argv: image.argv,
        start_time: stat.starttime,
        status: stat.status.as_str().into(),
        children,
    })
}

fn read_argv(pid: pid_t) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|data| {
            data.strip_suffix(&[0])
                .unwrap_or(&data)
                .split(|byte| *byte == 0)
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect()
        })
        .unwrap_or_default()
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
    pub fn proportional_set_bytes(pid: u32, expected_start_time: Option<u64>) -> Option<u64> {
        let before = LinuxProcessSnapshot::read_stat(pid as pid_t)?;
        if expected_start_time.is_some_and(|expected| before.starttime != expected) {
            return None;
        }
        let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
        let bytes = smaps.lines().find_map(|line| {
            let mut fields = line.strip_prefix("Pss:")?.split_whitespace();
            let kib = fields.next()?.parse::<u64>().ok()?;
            (fields.next()? == "kB").then_some(())?;
            kib.checked_mul(1024)
        })?;
        let after = LinuxProcessSnapshot::read_stat(pid as pid_t)?;
        (before.starttime == after.starttime).then_some(bytes)
    }

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

    /// The process tree rooted at `pid` as the background sampler last read
    /// it, at most about `SAMPLE_INTERVAL` plus one pass old. Never reads
    /// /proc on the calling thread, so it is safe on a thread serving other
    /// work. A root not looked up before is None until the sampler, woken by
    /// this lookup, has read it, and so is a process that has exited.
    pub fn with_root_pid_sampled(pid: u32) -> Option<Self> {
        TREE_SAMPLER.lookup(pid as pid_t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn proportional_memory_reads_kernel_accounting_and_rejects_reused_identity() {
        let pid = std::process::id();
        let start = LinuxProcessSnapshot::read_stat(pid as pid_t)
            .unwrap()
            .starttime;
        let bytes = LocalProcessInfo::proportional_set_bytes(pid, Some(start)).unwrap();
        assert!(bytes > 0);
        assert_eq!(bytes % 1024, 0);
        assert_eq!(
            LocalProcessInfo::proportional_set_bytes(pid, Some(start + 1)),
            None
        );
        assert_eq!(
            LocalProcessInfo::proportional_set_bytes(u32::MAX, None),
            None
        );
    }

    #[test]
    fn cached_process_lookups_share_one_proc_snapshot() {
        let _lock = SNAPSHOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
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

    /// Serializes the tests that pause the process-wide sampler.
    static SAMPLER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// A child process killed and reaped when dropped.
    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Lets a paused sampler finish its pass when dropped, so a failed
    /// assertion cannot leave it waiting.
    struct ResumeSampler(Arc<std::sync::Barrier>);

    impl Drop for ResumeSampler {
        fn drop(&mut self) {
            *SAMPLE_PAUSE.lock().unwrap() = None;
            self.0.wait();
        }
    }

    fn spawn(script: &str) -> Child {
        Child(
            std::process::Command::new("sh")
                .args(["-c", script])
                .spawn()
                .unwrap(),
        )
    }

    /// Looks `pid` up on another thread, failing if the lookup blocks.
    fn sampled(pid: u32) -> Option<LocalProcessInfo> {
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sent.send(LocalProcessInfo::with_root_pid_sampled(pid));
        });
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("a sampled lookup blocked")
    }

    fn sampled_eventually(
        pid: u32,
        done: impl Fn(&Option<LocalProcessInfo>) -> bool,
    ) -> Option<LocalProcessInfo> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = sampled(pid);
            if done(&found) {
                return found;
            }
            assert!(Instant::now() < deadline, "the sampler did not catch up");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_sampler_reads_a_trees_processes_and_their_exit() {
        let mut child = spawn("sleep 30 & wait");
        let pid = child.0.id();
        let root = sampled_eventually(pid, |found| {
            found
                .as_ref()
                .is_some_and(|root| root.children.values().any(|c| c.name == "sleep"))
        })
        .unwrap();
        assert_eq!(root.pid, pid);
        assert_eq!(root.name, "sh");
        let sleep = root.children.values().next().unwrap();
        assert_eq!(sleep.argv, ["sleep", "30"]);
        assert_eq!(sleep.executable.file_name().unwrap(), "sleep");

        child.0.kill().unwrap();
        child.0.wait().unwrap();
        sampled_eventually(pid, Option::is_none);
    }

    #[test]
    fn a_sampled_lookup_never_waits_for_the_sampler_or_reads_proc() {
        let _lock = SAMPLER_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _snapshot_lock = SNAPSHOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let child = spawn("sleep 30");
        let pid = child.0.id();
        sampled_eventually(pid, Option::is_some);
        SNAPSHOT_CAPTURE_COUNT.store(0, Ordering::Relaxed);

        // While the sampler is held mid-pass, lookups answer from what it
        // published last: a known root, and nothing for a new or dead one.
        let pause = Arc::new(std::sync::Barrier::new(2));
        *SAMPLE_PAUSE.lock().unwrap() = Some(Arc::clone(&pause));
        pause.wait();
        let resume = ResumeSampler(pause);
        assert_eq!(sampled(pid).unwrap().pid, pid);
        let other = spawn("sleep 30");
        assert!(sampled(other.0.id()).is_none());
        assert!(sampled(u32::MAX >> 2).is_none());
        drop(resume);

        sampled_eventually(other.0.id(), Option::is_some);
        // Only the sampler read /proc: no lookup captured a snapshot.
        assert_eq!(SNAPSHOT_CAPTURE_COUNT.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_capture_in_progress_does_not_hold_up_a_cached_lookup() {
        let _lock = SNAPSHOT_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let pid = std::process::id();
        cached_process_snapshot(Duration::ZERO);

        let pause = Arc::new(std::sync::Barrier::new(2));
        *CAPTURE_PAUSE.lock().unwrap() = Some(Arc::clone(&pause));
        let refresh = std::thread::spawn(|| {
            cached_process_snapshot(Duration::ZERO);
        });
        pause.wait();
        *CAPTURE_PAUSE.lock().unwrap() = None;

        let (looked_up, lookup) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = looked_up.send(LocalProcessInfo::with_root_pid_cached(
                pid,
                Duration::from_secs(3600),
            ));
        });
        let result = lookup.recv_timeout(Duration::from_secs(5));
        pause.wait();
        refresh.join().unwrap();

        let process = result.expect("the cached lookup waited for the capture");
        assert_eq!(process.unwrap().pid, pid);
    }
}
