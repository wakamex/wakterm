#![cfg(unix)]

use serde::Deserialize;
use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Deserialize)]
struct AgentWatchEvent {
    name: String,
    harness: String,
    transport: String,
    status: String,
    turn_state: String,
    observer_hint: Option<String>,
    session_path: Option<String>,
    message: String,
}

#[derive(Debug)]
struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        let child = command.spawn().unwrap_or_else(|err| {
            panic!("failed to spawn {:?}: {err}", command);
        });
        Self { child }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct SmokeHarness {
    harness: &'static str,
    name: String,
    expected_text: String,
    launch_cmd: Option<String>,
}

impl SmokeHarness {
    fn prompt(&self) -> String {
        format!(
            "Reply with exactly the following text and nothing else: {}",
            self.expected_text
        )
    }
}

#[test]
#[ignore = "requires locally installed/authenticated claude, codex, gemini, and opencode harnesses"]
fn agent_watch_smoke_with_real_harnesses_headless() {
    let runtime = tempfile::tempdir().expect("tempdir");
    let socket = runtime.path().join("wakterm/sock");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let suffix = unique_suffix();
    let workspace = format!("agent-watch-smoke-{suffix}");
    let harnesses = vec![
        SmokeHarness {
            harness: "claude",
            name: format!("smoke-claude-{suffix}"),
            expected_text: format!("claude smoke {suffix}"),
            launch_cmd: None,
        },
        SmokeHarness {
            harness: "codex",
            name: format!("smoke-codex-{suffix}"),
            expected_text: format!("codex smoke {suffix}"),
            launch_cmd: None,
        },
        SmokeHarness {
            harness: "gemini",
            name: format!("smoke-gemini-{suffix}"),
            expected_text: format!("gemini smoke {suffix}"),
            launch_cmd: None,
        },
        SmokeHarness {
            harness: "opencode",
            name: format!("smoke-opencode-{suffix}"),
            expected_text: format!("opencode smoke {suffix}"),
            launch_cmd: Some(format!("opencode -m {}", first_free_opencode_model())),
        },
    ];

    let _server = start_mux_server(runtime.path());
    wait_for_mux_ready(&socket);

    let (watcher, events) = start_agent_watch(&socket);
    let _watcher = watcher;
    let mut recent_events = vec![];

    for harness in &harnesses {
        let mut start_args = vec![
            "agent".to_string(),
            "start".to_string(),
            harness.harness.to_string(),
            "--new-window".to_string(),
            "--workspace".to_string(),
            workspace.clone(),
            "--cwd".to_string(),
            workspace_root
                .to_str()
                .expect("utf8 workspace path")
                .to_string(),
            "--name".to_string(),
            harness.name.clone(),
        ];
        if let Some(cmd) = &harness.launch_cmd {
            start_args.push("--cmd".to_string());
            start_args.push(cmd.clone());
        }
        let start = run_cli_json(&socket, workspace_root, start_args);
        assert_eq!(
            start["metadata"]["name"].as_str(),
            Some(harness.name.as_str()),
            "start result for {}: {start}",
            harness.harness
        );
    }

    for harness in &harnesses {
        let startup = wait_for_watch_event(
            &events,
            &mut recent_events,
            Duration::from_secs(45),
            |event| event.name == harness.name && event.status == "idle",
        );
        assert_eq!(startup.harness, harness.harness);
        assert_eq!(startup.transport, "pty");
        assert!(
            startup.observer_hint.is_some(),
            "startup event for {} missing observer hint: {startup:?}",
            harness.harness
        );
        assert!(
            startup.session_path.is_none(),
            "startup event for {} should not have a session yet: {startup:?}",
            harness.harness
        );
    }

    for harness in &harnesses {
        let prompt = harness.prompt();
        let send = run_cli_json(
            &socket,
            workspace_root,
            [
                "agent",
                "send",
                harness.name.as_str(),
                "--ack-timeout-ms",
                "30000",
                prompt.as_str(),
            ],
        );
        assert_eq!(
            send["acknowledgement"]["kind"].as_str(),
            Some("session_observer"),
            "send acknowledgement for {}: {send}",
            harness.harness
        );
        assert_eq!(
            send["acknowledgement"]["acknowledged"].as_bool(),
            Some(true),
            "send acknowledgement for {}: {send}",
            harness.harness
        );
        assert!(
            send["acknowledgement"]["session_path"].as_str().is_some(),
            "send acknowledgement for {} missing session path: {send}",
            harness.harness
        );
    }

    for harness in &harnesses {
        let completion = wait_for_watch_event(
            &events,
            &mut recent_events,
            Duration::from_secs(90),
            |event| {
                event.name == harness.name
                    && event.transport == "observed-pty"
                    && event.turn_state == "waiting-user"
                    && event
                        .session_path
                        .as_deref()
                        .map(|path| !path.is_empty())
                        .unwrap_or(false)
                    && event.message.contains(&harness.expected_text)
            },
        );
        assert_eq!(completion.harness, harness.harness);
    }
}

fn start_mux_server(runtime_dir: &Path) -> ChildGuard {
    let mut command = Command::new(mux_server_bin());
    command
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    ChildGuard::spawn(&mut command)
}

fn start_agent_watch(socket: &Path) -> (ChildGuard, Receiver<AgentWatchEvent>) {
    let mut command = base_cli_command(
        socket,
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap(),
    );
    command
        .args(["agent", "watch", "--format", "json", "--poll-ms", "100"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut watcher = ChildGuard::spawn(&mut command);
    let stdout = watcher
        .child
        .stdout
        .take()
        .expect("watch stdout to be piped");
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AgentWatchEvent>(&line) {
                Ok(event) => {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("failed to decode agent watch event: {err}: {line}");
                }
            }
        }
    });

    (watcher, rx)
}

fn wait_for_mux_ready(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();

    while Instant::now() < deadline {
        let status = base_cli_command(socket, workspace_root)
            .arg("list")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if matches!(status, Ok(status) if status.success()) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }

    panic!("mux server did not become ready at {}", socket.display());
}

fn wait_for_watch_event<F>(
    events: &Receiver<AgentWatchEvent>,
    recent_events: &mut Vec<AgentWatchEvent>,
    timeout: Duration,
    predicate: F,
) -> AgentWatchEvent
where
    F: Fn(&AgentWatchEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) => {
                recent_events.push(event.clone());
                if recent_events.len() > 64 {
                    recent_events.drain(0..recent_events.len() - 64);
                }
                if predicate(&event) {
                    return event;
                }
            }
            Err(err) => {
                panic!(
                    "timed out waiting for agent watch event: {}; recent events: {:#?}",
                    err, recent_events
                );
            }
        }
    }

    panic!(
        "timed out waiting for agent watch event; recent events: {:#?}",
        recent_events
    );
}

fn run_cli_json<I, S>(socket: &Path, current_dir: &Path, args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = base_cli_command(socket, current_dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run wakterm cli");
    assert!(
        output.status.success(),
        "wakterm cli failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "failed to parse wakterm cli json: {err}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[derive(Debug, Deserialize)]
struct OpencodeModelCost {
    input: f64,
    output: f64,
}

#[derive(Debug, Deserialize)]
struct OpencodeVerboseModel {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    cost: OpencodeModelCost,
}

fn first_free_opencode_model() -> String {
    let output = Command::new("opencode")
        .args(["models", "--verbose"])
        .stdin(Stdio::null())
        .output()
        .expect("run `opencode models --verbose`");
    assert!(
        output.status.success(),
        "`opencode models --verbose` failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 opencode models output");
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        let candidate = line.trim();
        if candidate.is_empty() || candidate.starts_with('{') {
            continue;
        }

        let mut json_lines = Vec::new();
        let mut depth = 0usize;
        while let Some(next) = lines.peek() {
            let trimmed = next.trim();
            if trimmed.is_empty() {
                lines.next();
                continue;
            }
            if depth == 0 && !trimmed.starts_with('{') {
                break;
            }

            let current = lines.next().expect("peeked line");
            depth += current.chars().filter(|&c| c == '{').count();
            depth -= current.chars().filter(|&c| c == '}').count();
            json_lines.push(current);
            if depth == 0 && !json_lines.is_empty() {
                break;
            }
        }

        if json_lines.is_empty() {
            continue;
        }

        let model: OpencodeVerboseModel = serde_json::from_str(&json_lines.join("\n"))
            .unwrap_or_else(|err| {
                panic!(
                    "failed to parse opencode model block for {}: {}",
                    candidate, err
                )
            });
        if model.cost.input == 0.0 && model.cost.output == 0.0 {
            return format!("{}/{}", model.provider_id, model.id);
        }
    }

    panic!("no free OpenCode model found in `opencode models --verbose` output");
}

fn base_cli_command(socket: &Path, current_dir: &Path) -> Command {
    let mut command = Command::new(wakterm_bin());
    command
        .current_dir(current_dir)
        .env_remove("WAKTERM_PANE")
        .env("WAKTERM_UNIX_SOCKET", socket)
        .args(["cli", "--prefer-mux", "--no-auto-start"]);
    command
}

fn wakterm_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wakterm"))
}

fn mux_server_bin() -> PathBuf {
    if let Some(path) = env::var_os("WAKTERM_MUX_SERVER_BIN") {
        return PathBuf::from(path);
    }

    let sibling =
        wakterm_bin().with_file_name(format!("wakterm-mux-server{}", env::consts::EXE_SUFFIX));
    if sibling.exists() {
        return sibling;
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let fallback = workspace_root
        .join("target")
        .join("debug")
        .join(format!("wakterm-mux-server{}", env::consts::EXE_SUFFIX));
    if fallback.exists() {
        return fallback;
    }

    panic!(
        "unable to locate wakterm-mux-server; build it first with `cargo build -p wakterm-mux-server` or set WAKTERM_MUX_SERVER_BIN"
    );
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock drift")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}
