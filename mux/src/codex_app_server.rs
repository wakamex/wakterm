use crate::agent::{
    finalize_runtime_snapshot, AgentObservedTurn, AgentObservedTurnOutcome, AgentTransport,
    AgentTurnState, CodexAppServerSession,
};
use crate::Mux;
use anyhow::{bail, Context};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wakterm_uds::{UnixListener, UnixStream};

const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

fn initialize_params() -> Value {
    json!({
        "clientInfo": {"name": "wakterm", "title": "Wakterm", "version": config::wakterm_version()},
        "capabilities": {
            "experimentalApi": true,
            "optOutNotificationMethods": ["app/list/updated"]
        }
    })
}

fn apply_tui_settings(params: &mut serde_json::Map<String, Value>, args: &[String]) {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let (name, inline_value) = arg
            .split_once('=')
            .map(|(name, value)| (name, Some(value)))
            .unwrap_or((arg, None));
        match name {
            "-a" | "--ask-for-approval" => {
                if let Some(value) =
                    inline_value.or_else(|| args.get(index + 1).map(String::as_str))
                {
                    params.insert(
                        "approvalPolicy".to_string(),
                        Value::String(value.to_string()),
                    );
                }
                if inline_value.is_none() {
                    index += 1;
                }
            }
            "-s" | "--sandbox" => {
                if let Some(value) =
                    inline_value.or_else(|| args.get(index + 1).map(String::as_str))
                {
                    params.insert("sandbox".to_string(), Value::String(value.to_string()));
                }
                if inline_value.is_none() {
                    index += 1;
                }
            }
            "--dangerously-bypass-approvals-and-sandbox" => {
                params.insert(
                    "approvalPolicy".to_string(),
                    Value::String("never".to_string()),
                );
                params.insert(
                    "sandbox".to_string(),
                    Value::String("danger-full-access".to_string()),
                );
            }
            _ => {}
        }
        index += 1;
    }
}

fn metadata_only_resume_params(thread_id: &str, cwd: &str, tui_args: &[String]) -> Value {
    // Wakterm follows live notifications and does not consume hydrated turn
    // history. Keeping it out of resume responses also bounds frame size.
    let mut params = json!({
        "threadId": thread_id,
        "cwd": cwd,
        "excludeTurns": true,
        "initialTurnsPage": {
            "limit": 2,
            "sortDirection": "desc",
            "itemsView": "notLoaded"
        }
    });
    apply_tui_settings(params.as_object_mut().unwrap(), tui_args);
    params
}

fn thread_start_params(cwd: &str, tui_args: &[String]) -> Value {
    let mut params = json!({"cwd": cwd, "serviceName": "wakterm"});
    apply_tui_settings(params.as_object_mut().unwrap(), tui_args);
    params
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PrepareCodexLaunch {
    pub name: String,
    pub cwd: String,
    pub resume_thread_id: Option<String>,
    pub tui_args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreparedCodexLaunch {
    pub argv: Vec<String>,
    pub session: CodexAppServerSession,
}

#[derive(Clone, Debug)]
pub struct RecoveryThread {
    pub name: String,
    pub cwd: String,
    pub session: CodexAppServerSession,
}

struct Connection {
    writer: Mutex<UnixStream>,
    pending: Mutex<HashMap<u64, Sender<anyhow::Result<Value>>>>,
    next_id: AtomicU64,
}

impl Connection {
    fn connect(socket_path: &Path) -> anyhow::Result<Arc<Self>> {
        let mut stream = UnixStream::connect(socket_path)
            .with_context(|| format!("connecting to {}", socket_path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let key = base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        write!(
            stream,
            "GET /rpc HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n"
        )?;
        stream.flush()?;
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            if response.len() > 8192 {
                bail!("Codex app-server WebSocket response header is too large");
            }
            stream.read_exact(&mut byte)?;
            response.push(byte[0]);
        }
        let response = String::from_utf8_lossy(&response);
        if !response.starts_with("HTTP/1.1 101 ") {
            bail!(
                "Codex app-server rejected WebSocket upgrade: {}",
                response.trim()
            );
        }
        stream.set_read_timeout(None)?;
        let reader = stream.try_clone()?;
        let connection = Arc::new(Self {
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let weak = Arc::downgrade(&connection);
        thread::spawn(move || {
            if let Err(err) = read_messages(reader, |opcode, payload| {
                if let Some(connection) = weak.upgrade() {
                    match opcode {
                        1 => match serde_json::from_slice(&payload) {
                            Ok(message) => connection.dispatch(message),
                            Err(err) => {
                                log::error!("invalid Codex app-server message: {err:#}");
                                return false;
                            }
                        },
                        9 => {
                            if let Err(err) = connection.send_payload(10, &payload) {
                                log::error!("failed to answer Codex app-server ping: {err:#}");
                                return false;
                            }
                        }
                        _ => {}
                    }
                    true
                } else {
                    false
                }
            }) {
                log::error!("Codex app-server connection closed: {err:#}");
            }
            if let Some(connection) = weak.upgrade() {
                let mut pending = connection.pending.lock();
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(anyhow::anyhow!("Codex app-server connection closed")));
                }
            }
            if let Some(mux) = Mux::try_get() {
                mux.codex_app_server_disconnected();
            }
        });
        Ok(connection)
    }

    fn initialize(&self) -> anyhow::Result<()> {
        self.request("initialize", initialize_params())?;
        self.notify("initialized", json!({}))
    }

    fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().insert(id, tx);
        if let Err(err) = self.send(&json!({"id": id, "method": method, "params": params})) {
            self.pending.lock().remove(&id);
            return Err(err);
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(err) => {
                self.pending.lock().remove(&id);
                Err(err).context("timed out waiting for Codex app-server response")?
            }
        }
    }

    fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(&json!({"method": method, "params": params}))
    }

    fn send(&self, value: &Value) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(value)?;
        self.send_payload(1, &payload)
    }

    fn send_payload(&self, opcode: u8, payload: &[u8]) -> anyhow::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        let mask_source = uuid::Uuid::new_v4();
        let mask = &mask_source.as_bytes()[..4];
        match payload.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len if len <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        let mut writer = self.writer.lock();
        writer.write_all(&frame)?;
        writer.flush()?;
        Ok(())
    }

    fn dispatch(&self, message: Value) {
        if message.get("method").is_none() {
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                return;
            };
            if let Some(tx) = self.pending.lock().remove(&id) {
                let result = match message.get("error") {
                    Some(error) => Err(anyhow::anyhow!("{}", error)),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
                return;
            }
            return;
        }
        if let Some(mux) = Mux::try_get() {
            mux.apply_codex_app_server_notification(&message);
        }
    }
}

pub struct CodexAppServer {
    state: Mutex<State>,
    thread_runtime_seed_by_id: Mutex<HashMap<String, ThreadRuntimeSeed>>,
    next_tui_proxy_id: AtomicU64,
    tui_proxy_paths: Mutex<Vec<PathBuf>>,
}

#[derive(Clone, Debug, PartialEq)]
struct ThreadRuntimeSeed {
    status: Value,
    last_turn_completed_at: Option<DateTime<Utc>>,
}

struct State {
    child: Option<Child>,
    connection: Option<Arc<Connection>>,
    executable: Option<String>,
    version: Option<String>,
    socket_path: PathBuf,
    recovered_once: bool,
}

impl CodexAppServer {
    pub fn new(mux_instance_id: usize) -> Self {
        Self {
            state: Mutex::new(State {
                child: None,
                connection: None,
                executable: None,
                version: None,
                socket_path: config::RUNTIME_DIR.join(format!(
                    "codex-app-server-{}-{mux_instance_id}.sock",
                    std::process::id()
                )),
                recovered_once: false,
            }),
            thread_runtime_seed_by_id: Mutex::new(HashMap::new()),
            next_tui_proxy_id: AtomicU64::new(1),
            tui_proxy_paths: Mutex::new(Vec::new()),
        }
    }

    pub fn prepare(&self, request: PrepareCodexLaunch) -> anyhow::Result<PreparedCodexLaunch> {
        validate_tui_args(&request.tui_args)?;
        validate_resume_thread_id(request.resume_thread_id.as_deref())?;
        let mut state = self.state.lock();
        ensure_running(&mut state)?;
        self.prepare_connected(&mut state, request, true)
    }

    pub fn attach_existing(
        &self,
        request: PrepareCodexLaunch,
        expected_endpoint: &str,
    ) -> anyhow::Result<PreparedCodexLaunch> {
        validate_tui_args(&request.tui_args)?;
        validate_resume_thread_id(request.resume_thread_id.as_deref())?;
        anyhow::ensure!(
            request.resume_thread_id.is_some(),
            "connected Codex promotion requires an exact thread UUID"
        );
        let mut state = self.state.lock();
        anyhow::ensure!(
            codex_socket_url(&state.socket_path) == expected_endpoint,
            "live Codex TUI is connected to a different app-server endpoint"
        );
        let child_alive = state
            .child
            .as_mut()
            .map(|child| child.try_wait().map(|status| status.is_none()))
            .transpose()?
            .unwrap_or(false);
        anyhow::ensure!(
            child_alive && state.connection.is_some(),
            "mux-owned Codex app-server is not currently connected"
        );
        self.prepare_connected(&mut state, request, false)
    }

    fn prepare_connected(
        &self,
        state: &mut State,
        request: PrepareCodexLaunch,
        proxy_tui: bool,
    ) -> anyhow::Result<PreparedCodexLaunch> {
        let connection = state
            .connection
            .as_ref()
            .context("Codex app-server connection missing")?;
        let result = if let Some(thread_id) = request.resume_thread_id.as_deref() {
            connection.request(
                "thread/resume",
                metadata_only_resume_params(thread_id, &request.cwd, &request.tui_args),
            )?
        } else {
            connection.request(
                "thread/start",
                thread_start_params(&request.cwd, &request.tui_args),
            )?
        };
        let thread = result
            .get("thread")
            .context("Codex response omitted thread")?;
        let thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .context("Codex response omitted thread.id")?
            .to_string();
        let session_id = thread
            .get("sessionId")
            .and_then(Value::as_str)
            .context("Codex response omitted thread.sessionId")?
            .to_string();
        self.record_thread_bootstrap(&result);
        if let Some(expected) = request.resume_thread_id.as_deref() {
            anyhow::ensure!(
                thread_id == expected,
                "Codex resumed thread {thread_id}, expected {expected}"
            );
        }
        // The installed app-server does not make a new empty thread resumable
        // from another connection until it has durable state. Naming is a
        // supported, model-free write that makes the native TUI resume exact.
        connection.request(
            "thread/name/set",
            json!({"threadId": thread_id, "name": request.name}),
        )?;
        let executable = state.executable.clone().unwrap();
        let version = state.version.clone().unwrap();
        let endpoint = if proxy_tui {
            self.start_tui_proxy(&state.socket_path, &thread_id)?
        } else {
            codex_socket_url(&state.socket_path)
        };
        let mut native_argv = vec![
            executable.clone(),
            "resume".to_string(),
            "--remote".to_string(),
            endpoint,
            "-C".to_string(),
            request.cwd.clone(),
        ];
        native_argv.push(thread_id.clone());
        native_argv.extend(request.tui_args.clone());
        let argv = native_tui_argv(&native_argv);
        Ok(PreparedCodexLaunch {
            argv,
            session: CodexAppServerSession {
                thread_id,
                session_id,
                executable,
                version,
                tui_args: request.tui_args,
            },
        })
    }

    fn start_tui_proxy(&self, upstream_path: &Path, thread_id: &str) -> anyhow::Result<String> {
        let proxy_id = self.next_tui_proxy_id.fetch_add(1, Ordering::Relaxed);
        let proxy_path =
            config::RUNTIME_DIR.join(format!("codex-tui-{}-{proxy_id}.sock", std::process::id()));
        if proxy_path.exists() {
            std::fs::remove_file(&proxy_path)
                .with_context(|| format!("removing stale {}", proxy_path.display()))?;
        }
        let listener = UnixListener::bind(&proxy_path)
            .with_context(|| format!("binding Codex TUI proxy {}", proxy_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&proxy_path, std::fs::Permissions::from_mode(0o600))?;
        }
        self.tui_proxy_paths.lock().push(proxy_path.clone());
        let upstream_path = upstream_path.to_path_buf();
        let current_thread_id = Arc::new(Mutex::new(thread_id.to_string()));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let client = match stream {
                    Ok(client) => client,
                    Err(err) => {
                        log::warn!("Codex TUI proxy accept failed: {err:#}");
                        break;
                    }
                };
                let upstream_path = upstream_path.clone();
                let current_thread_id = Arc::clone(&current_thread_id);
                thread::spawn(move || {
                    if let Err(err) =
                        proxy_tui_connection(client, &upstream_path, current_thread_id)
                    {
                        log::warn!("Codex TUI proxy connection closed: {err:#}");
                    }
                });
            }
        });
        Ok(codex_socket_url(&proxy_path))
    }

    pub fn mark_disconnected(&self) {
        self.state.lock().connection = None;
        self.thread_runtime_seed_by_id.lock().clear();
    }

    pub fn unsubscribe(&self, thread_id: &str) {
        self.thread_runtime_seed_by_id.lock().remove(thread_id);
        let connection = self.state.lock().connection.clone();
        let thread_id = thread_id.to_string();
        if let Some(connection) = connection {
            thread::spawn(move || {
                if let Err(err) =
                    connection.request("thread/unsubscribe", json!({"threadId": thread_id}))
                {
                    log::warn!("Could not unsubscribe from Codex thread: {err:#}");
                }
            });
        }
    }

    pub fn recover(&self, threads: &[RecoveryThread]) -> anyhow::Result<HashMap<String, String>> {
        let mut state = self.state.lock();
        anyhow::ensure!(
            !state.recovered_once,
            "automatic Codex app-server recovery was already attempted"
        );
        state.recovered_once = true;
        ensure_running(&mut state)?;
        let connection = state
            .connection
            .as_ref()
            .context("Codex app-server connection missing after restart")?;
        let mut failures = HashMap::new();
        for thread in threads {
            let recovered = (|| {
                let result = connection.request(
                    "thread/resume",
                    metadata_only_resume_params(
                        &thread.session.thread_id,
                        &thread.cwd,
                        &thread.session.tui_args,
                    ),
                )?;
                let restored = result
                    .get("thread")
                    .context("Codex response omitted thread")?;
                anyhow::ensure!(
                    restored.get("id").and_then(Value::as_str)
                        == Some(thread.session.thread_id.as_str()),
                    "Codex app-server recovered a different thread"
                );
                anyhow::ensure!(
                    restored.get("sessionId").and_then(Value::as_str)
                        == Some(thread.session.session_id.as_str()),
                    "Codex app-server recovered a different session"
                );
                self.record_thread_bootstrap(&result);
                connection.request(
                    "thread/name/set",
                    json!({"threadId": thread.session.thread_id, "name": thread.name}),
                )?;
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(err) = recovered {
                failures.insert(thread.session.thread_id.clone(), format!("{err:#}"));
            }
        }
        Ok(failures)
    }

    pub(crate) fn record_thread_bootstrap(&self, result: &Value) {
        let Some(thread) = result.get("thread") else {
            return;
        };
        let Some(thread_id) = thread.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(status) = thread.get("status") else {
            return;
        };
        self.thread_runtime_seed_by_id.lock().insert(
            thread_id.to_string(),
            ThreadRuntimeSeed {
                status: status.clone(),
                last_turn_completed_at: newest_completed_turn_at(result),
            },
        );
    }

    pub(crate) fn record_notification(&self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str);
        let Some(thread_id) = message.pointer("/params/threadId").and_then(Value::as_str) else {
            return;
        };
        if method == Some("thread/closed") {
            self.thread_runtime_seed_by_id.lock().remove(thread_id);
            return;
        }
        if method == Some("turn/completed") {
            let completed_at = message
                .pointer("/params/turn/completedAt")
                .and_then(timestamp_seconds)
                .unwrap_or_else(Utc::now);
            if let Some(seed) = self.thread_runtime_seed_by_id.lock().get_mut(thread_id) {
                seed.last_turn_completed_at = Some(completed_at);
            }
            return;
        }
        if method != Some("thread/status/changed") {
            return;
        }
        let Some(status) = message.pointer("/params/status") else {
            return;
        };
        self.thread_runtime_seed_by_id
            .lock()
            .entry(thread_id.to_string())
            .and_modify(|seed| seed.status = status.clone())
            .or_insert_with(|| ThreadRuntimeSeed {
                status: status.clone(),
                last_turn_completed_at: None,
            });
    }

    pub(crate) fn prime_runtime(
        &self,
        thread_id: &str,
        runtime: &mut crate::agent::AgentRuntimeSnapshot,
    ) {
        let Some(seed) = self
            .thread_runtime_seed_by_id
            .lock()
            .get(thread_id)
            .cloned()
        else {
            return;
        };
        runtime.observed_at = Utc::now();
        apply_thread_status(runtime, &seed.status);
        runtime.last_turn_completed_at = seed.last_turn_completed_at;
        finalize_runtime_snapshot(runtime);
    }
}

fn timestamp_seconds(value: &Value) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(value.as_i64()?, 0).single()
}

fn newest_completed_turn_at(result: &Value) -> Option<DateTime<Utc>> {
    result
        .pointer("/initialTurnsPage/data")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|turn| turn.get("completedAt").and_then(timestamp_seconds))
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        state.connection = None;
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if state.socket_path.exists() {
            let _ = std::fs::remove_file(&state.socket_path);
        }
        for path in self.tui_proxy_paths.get_mut().drain(..) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn ensure_running(state: &mut State) -> anyhow::Result<()> {
    if state.connection.is_some()
        && state
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .flatten()
            .is_none()
    {
        return Ok(());
    }
    if let Some(mut child) = state.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    state.connection = None;
    let executable = which_codex()?;
    let output = Command::new(&executable)
        .arg("--version")
        .output()
        .context("reading Codex version")?;
    anyhow::ensure!(output.status.success(), "{} --version failed", executable);
    let version = String::from_utf8(output.stdout)?.trim().to_string();
    std::fs::create_dir_all(&*config::RUNTIME_DIR)?;
    if state.socket_path.exists() {
        std::fs::remove_file(&state.socket_path)
            .with_context(|| format!("removing stale {}", state.socket_path.display()))?;
    }
    let mut child = Command::new(&executable)
        .args(["app-server", "--listen"])
        .arg(codex_socket_url(&state.socket_path))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting shared Codex app-server")?;
    let startup = (|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !state.socket_path.exists() {
            anyhow::ensure!(
                child.try_wait()?.is_none(),
                "Codex app-server exited before its socket appeared"
            );
            anyhow::ensure!(
                Instant::now() < deadline,
                "Codex app-server socket did not appear"
            );
            thread::sleep(Duration::from_millis(25));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state.socket_path, std::fs::Permissions::from_mode(0o600))?;
        }
        let connection = Connection::connect(&state.socket_path)?;
        connection.initialize()?;
        Ok::<_, anyhow::Error>(connection)
    })();
    let connection = match startup {
        Ok(connection) => connection,
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&state.socket_path);
            return Err(err);
        }
    };
    state.child = Some(child);
    state.connection = Some(connection);
    state.executable = Some(executable);
    state.version = Some(version);
    Ok(())
}

fn which_codex() -> anyhow::Result<String> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    for directory in std::env::split_paths(&path) {
        for candidate in codex_executable_candidates(&directory) {
            if candidate.is_file() {
                return Ok(candidate.to_string_lossy().to_string());
            }
        }
    }
    bail!("codex executable was not found in PATH")
}

#[cfg(not(windows))]
fn codex_executable_candidates(directory: &Path) -> Vec<PathBuf> {
    vec![directory.join("codex")]
}

#[cfg(windows)]
fn codex_executable_candidates(directory: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![directory.join("codex")];
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    candidates.extend(
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| directory.join(format!("codex{extension}"))),
    );
    candidates
}

fn validate_tui_args(args: &[String]) -> anyhow::Result<()> {
    const OWNED: &[&str] = &[
        "--remote",
        "--remote-auth-token-env",
        "--cd",
        "-C",
        "--last",
        "--all",
        "--enable",
        "--disable",
        "-c",
        "--config",
        "-p",
        "--profile",
    ];
    const FLAGS: &[&str] = &[
        "--include-non-interactive",
        "--strict-config",
        "--oss",
        "--approve-for-me",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--search",
        "--no-alt-screen",
    ];
    const VALUES: &[&str] = &[
        "-i",
        "--image",
        "-m",
        "--model",
        "--local-provider",
        "-s",
        "--sandbox",
        "--add-dir",
        "-a",
        "--ask-for-approval",
    ];
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        anyhow::ensure!(
            !OWNED.iter().any(|owned| {
                arg == owned
                    || arg.starts_with(&format!("{owned}="))
                    || (owned.len() == 2 && arg.starts_with(owned))
            }),
            "Codex option {arg} conflicts with Wakterm ownership"
        );
        if FLAGS.contains(&arg.as_str()) {
            index += 1;
            continue;
        }
        if VALUES.contains(&arg.as_str()) {
            anyhow::ensure!(
                index + 1 < args.len(),
                "Codex option {arg} requires a value"
            );
            index += 2;
            continue;
        }
        if VALUES
            .iter()
            .any(|option| arg.starts_with(&format!("{option}=")))
        {
            index += 1;
            continue;
        }
        bail!("unsupported Codex option {arg}");
    }
    Ok(())
}

fn validate_resume_thread_id(thread_id: Option<&str>) -> anyhow::Result<()> {
    let Some(thread_id) = thread_id else {
        return Ok(());
    };
    let parsed =
        uuid::Uuid::parse_str(thread_id).context("--resume must be an exact Codex thread UUID")?;
    anyhow::ensure!(
        parsed.to_string() == thread_id,
        "--resume must use the canonical exact Codex thread UUID"
    );
    Ok(())
}

fn codex_socket_url(path: &Path) -> String {
    #[cfg(windows)]
    let path = path.to_string_lossy().replace('\\', "/");
    #[cfg(not(windows))]
    let path = path.to_string_lossy();
    format!("unix://{path}")
}

#[cfg(unix)]
fn native_tui_argv(native_argv: &[String]) -> Vec<String> {
    native_argv.to_vec()
}

#[cfg(windows)]
fn native_tui_argv(native_argv: &[String]) -> Vec<String> {
    let invocation = native_argv
        .iter()
        .map(|arg| powershell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!(
        "& {invocation}; if ($LASTEXITCODE -ne 0) {{ Start-Sleep -Seconds 1; & {invocation} }}; exit $LASTEXITCODE"
    );
    let encoded = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    vec![
        "powershell.exe".to_string(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-EncodedCommand".to_string(),
        base64::engine::general_purpose::STANDARD.encode(encoded),
    ]
}

#[cfg(windows)]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn read_messages(
    mut stream: UnixStream,
    mut handle: impl FnMut(u8, Vec<u8>) -> bool,
) -> anyhow::Result<()> {
    let mut fragmented_opcode = None;
    let mut fragmented_payload = Vec::new();
    loop {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header)?;
        let opcode = header[0] & 0x0f;
        let finished = header[0] & 0x80 != 0;
        let mut len = (header[1] & 0x7f) as usize;
        if len == 126 {
            let mut bytes = [0u8; 2];
            stream.read_exact(&mut bytes)?;
            len = u16::from_be_bytes(bytes) as usize;
        } else if len == 127 {
            let mut bytes = [0u8; 8];
            stream.read_exact(&mut bytes)?;
            len = usize::try_from(u64::from_be_bytes(bytes))?;
        }
        anyhow::ensure!(
            len <= MAX_MESSAGE_SIZE,
            "Codex app-server message exceeds {MAX_MESSAGE_SIZE} bytes"
        );
        let masked = header[1] & 0x80 != 0;
        let mut mask = [0u8; 4];
        if masked {
            stream.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        if opcode == 8 {
            return Ok(());
        }
        if opcode == 0 {
            anyhow::ensure!(
                fragmented_opcode.is_some(),
                "unexpected WebSocket continuation"
            );
            anyhow::ensure!(
                fragmented_payload.len() + payload.len() <= MAX_MESSAGE_SIZE,
                "Codex app-server fragmented message exceeds {MAX_MESSAGE_SIZE} bytes"
            );
            fragmented_payload.extend_from_slice(&payload);
            if finished {
                let opcode = fragmented_opcode.take().unwrap();
                if !handle(opcode, std::mem::take(&mut fragmented_payload)) {
                    return Ok(());
                }
            }
            continue;
        }
        if !finished && opcode < 8 {
            anyhow::ensure!(fragmented_opcode.is_none(), "nested WebSocket fragments");
            fragmented_opcode = Some(opcode);
            fragmented_payload = payload;
            continue;
        }
        if !handle(opcode, payload) {
            return Ok(());
        }
    }
}

#[derive(Clone, Debug)]
struct PendingTuiThreadChange {
    requested_thread_id: Option<String>,
}

impl PendingTuiThreadChange {
    fn accepts_thread(&self, thread_id: &str) -> bool {
        self.requested_thread_id
            .as_deref()
            .map(|requested| requested == thread_id)
            .unwrap_or(true)
    }
}

struct TuiProxyProtocolState {
    current_thread_id: String,
    switched_thread: bool,
    pending: HashMap<String, PendingTuiThreadChange>,
    buffered_notifications: Vec<Value>,
}

impl TuiProxyProtocolState {
    fn new(thread_id: &str) -> Self {
        Self {
            current_thread_id: thread_id.to_string(),
            switched_thread: false,
            pending: HashMap::new(),
            buffered_notifications: Vec::new(),
        }
    }

    fn record_client_message(&mut self, message: &Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        if !matches!(method, "thread/start" | "thread/resume" | "thread/fork") {
            return;
        }
        if method == "thread/start"
            && message
                .pointer("/params/ephemeral")
                .and_then(Value::as_bool)
                == Some(true)
        {
            return;
        }
        let Some(id) = json_rpc_id(message.get("id")) else {
            return;
        };
        self.pending.insert(
            id,
            PendingTuiThreadChange {
                requested_thread_id: message
                    .pointer("/params/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
        );
    }

    fn retain_buffered_for_pending(&mut self) {
        let remaining = self.pending.values().cloned().collect::<Vec<_>>();
        self.buffered_notifications.retain(|notification| {
            notification_thread_id(notification).is_some_and(|thread_id| {
                remaining
                    .iter()
                    .any(|pending| pending.accepts_thread(thread_id))
            })
        });
    }

    fn record_server_message(&mut self, message: &Value) -> TuiProxyDispatch {
        if message.get("method").is_some() {
            if let Some(thread_id) = notification_thread_id(message) {
                if thread_id == self.current_thread_id {
                    if self.switched_thread {
                        return TuiProxyDispatch {
                            transition: None,
                            notifications: vec![message.clone()],
                        };
                    }
                } else if self
                    .pending
                    .values()
                    .any(|pending| pending.accepts_thread(thread_id))
                {
                    self.buffered_notifications.push(message.clone());
                }
            }
            return TuiProxyDispatch::default();
        }

        let Some(id) = json_rpc_id(message.get("id")) else {
            return TuiProxyDispatch::default();
        };
        let Some(pending) = self.pending.remove(&id) else {
            return TuiProxyDispatch::default();
        };
        if message.get("error").is_some() {
            self.retain_buffered_for_pending();
            return TuiProxyDispatch::default();
        }
        let Some(thread) = message.pointer("/result/thread") else {
            self.retain_buffered_for_pending();
            return TuiProxyDispatch::default();
        };
        let Some(new_thread_id) = thread.get("id").and_then(Value::as_str) else {
            self.retain_buffered_for_pending();
            return TuiProxyDispatch::default();
        };
        if pending
            .requested_thread_id
            .as_deref()
            .is_some_and(|requested| requested != new_thread_id)
        {
            self.retain_buffered_for_pending();
            return TuiProxyDispatch::default();
        }
        if new_thread_id == self.current_thread_id {
            self.retain_buffered_for_pending();
            return TuiProxyDispatch::default();
        }
        let old_thread_id =
            std::mem::replace(&mut self.current_thread_id, new_thread_id.to_string());
        self.switched_thread = true;
        let session_id = thread
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or(new_thread_id)
            .to_string();
        let remaining = self.pending.values().cloned().collect::<Vec<_>>();
        let mut notifications = Vec::new();
        let mut still_buffered = Vec::new();
        for notification in std::mem::take(&mut self.buffered_notifications) {
            let notification_thread = notification_thread_id(&notification);
            if notification_thread == Some(new_thread_id) {
                notifications.push(notification);
            } else if notification_thread.is_some_and(|thread_id| {
                remaining
                    .iter()
                    .any(|pending| pending.accepts_thread(thread_id))
            }) {
                still_buffered.push(notification);
            }
        }
        self.buffered_notifications = still_buffered;
        TuiProxyDispatch {
            transition: Some(TuiThreadTransition {
                old_thread_id,
                new_thread_id: new_thread_id.to_string(),
                session_id,
                bootstrap: message.get("result").cloned().unwrap_or(Value::Null),
            }),
            notifications,
        }
    }
}

#[derive(Clone, Debug)]
struct TuiThreadTransition {
    old_thread_id: String,
    new_thread_id: String,
    session_id: String,
    bootstrap: Value,
}

#[derive(Default)]
struct TuiProxyDispatch {
    transition: Option<TuiThreadTransition>,
    notifications: Vec<Value>,
}

fn json_rpc_id(id: Option<&Value>) -> Option<String> {
    match id? {
        Value::String(id) => Some(format!("s:{id}")),
        Value::Number(id) => Some(format!("n:{id}")),
        _ => None,
    }
}

pub(crate) fn notification_thread_id(message: &Value) -> Option<&str> {
    message
        .pointer("/params/threadId")
        .or_else(|| message.pointer("/params/thread/id"))
        .and_then(Value::as_str)
}

fn dispatch_tui_proxy_messages(dispatch: TuiProxyDispatch) {
    if dispatch.transition.is_none() && dispatch.notifications.is_empty() {
        return;
    }
    promise::spawn::spawn_into_main_thread(async move {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        apply_tui_proxy_dispatch(&mux, dispatch);
    })
    .detach();
}

fn apply_tui_proxy_dispatch(mux: &Mux, dispatch: TuiProxyDispatch) {
    let current_thread_id = if let Some(transition) = dispatch.transition {
        let new_thread_id = transition.new_thread_id.clone();
        if let Err(err) = mux.rebind_codex_app_server_pane(
            &transition.old_thread_id,
            &transition.new_thread_id,
            &transition.session_id,
            &transition.bootstrap,
        ) {
            log::error!("could not follow Codex TUI thread change: {err:#}");
            return;
        }
        new_thread_id
    } else {
        dispatch
            .notifications
            .first()
            .and_then(notification_thread_id)
            .unwrap_or_default()
            .to_string()
    };
    for notification in dispatch.notifications {
        mux.apply_codex_tui_notification(&current_thread_id, &notification);
    }
}

fn proxy_tui_connection(
    mut client: UnixStream,
    upstream_path: &Path,
    current_thread_id: Arc<Mutex<String>>,
) -> anyhow::Result<()> {
    let mut upstream = UnixStream::connect(upstream_path)
        .with_context(|| format!("connecting TUI proxy to {}", upstream_path.display()))?;
    relay_http_header(&mut client, &mut upstream)?;
    relay_http_header(&mut upstream, &mut client)?;

    let state = Arc::new(Mutex::new(TuiProxyProtocolState::new(
        &current_thread_id.lock(),
    )));
    let client_reader = client.try_clone()?;
    let mut upstream_writer = upstream.try_clone()?;
    let client_state = Arc::clone(&state);
    let client_to_upstream = thread::spawn(move || {
        let result = relay_websocket_frames(client_reader, &mut upstream_writer, |message| {
            client_state.lock().record_client_message(message);
        });
        let _ = upstream_writer.shutdown(Shutdown::Both);
        result
    });

    let result = relay_websocket_frames(upstream, &mut client, |message| {
        let dispatch = state.lock().record_server_message(message);
        if let Some(transition) = dispatch.transition.as_ref() {
            *current_thread_id.lock() = transition.new_thread_id.clone();
        }
        dispatch_tui_proxy_messages(dispatch);
    });
    let _ = client.shutdown(Shutdown::Both);
    let _ = client_to_upstream.join();
    result
}

fn relay_http_header(reader: &mut UnixStream, writer: &mut UnixStream) -> anyhow::Result<()> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(header.len() < 8192, "WebSocket HTTP header is too large");
        reader.read_exact(&mut byte)?;
        header.push(byte[0]);
    }
    writer.write_all(&header)?;
    writer.flush()?;
    Ok(())
}

fn relay_websocket_frames(
    mut reader: UnixStream,
    writer: &mut UnixStream,
    mut inspect: impl FnMut(&Value),
) -> anyhow::Result<()> {
    let mut fragmented_payload = Vec::new();
    let mut fragmented_opcode = None;
    loop {
        let mut header = [0u8; 2];
        reader.read_exact(&mut header)?;
        let finished = header[0] & 0x80 != 0;
        let opcode = header[0] & 0x0f;
        let mut raw = header.to_vec();
        let mut len = (header[1] & 0x7f) as usize;
        if len == 126 {
            let mut bytes = [0u8; 2];
            reader.read_exact(&mut bytes)?;
            raw.extend_from_slice(&bytes);
            len = u16::from_be_bytes(bytes) as usize;
        } else if len == 127 {
            let mut bytes = [0u8; 8];
            reader.read_exact(&mut bytes)?;
            raw.extend_from_slice(&bytes);
            len = usize::try_from(u64::from_be_bytes(bytes))?;
        }
        anyhow::ensure!(
            len <= MAX_MESSAGE_SIZE,
            "Codex app-server message exceeds {MAX_MESSAGE_SIZE} bytes"
        );
        let masked = header[1] & 0x80 != 0;
        let mut mask = [0u8; 4];
        if masked {
            reader.read_exact(&mut mask)?;
            raw.extend_from_slice(&mask);
        }
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        raw.extend_from_slice(&payload);
        writer.write_all(&raw)?;
        writer.flush()?;

        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        if opcode == 8 {
            return Ok(());
        }
        if opcode == 0 {
            let Some(fragmented_opcode_value) = fragmented_opcode else {
                bail!("unexpected WebSocket continuation");
            };
            anyhow::ensure!(
                fragmented_payload.len() + payload.len() <= MAX_MESSAGE_SIZE,
                "Codex app-server fragmented message exceeds {MAX_MESSAGE_SIZE} bytes"
            );
            fragmented_payload.extend_from_slice(&payload);
            if finished {
                if fragmented_opcode_value == 1 {
                    if let Ok(message) = serde_json::from_slice(&fragmented_payload) {
                        inspect(&message);
                    }
                }
                fragmented_payload.clear();
                fragmented_opcode = None;
            }
        } else if !finished && opcode < 8 {
            fragmented_opcode = Some(opcode);
            fragmented_payload = payload;
        } else if opcode == 1 {
            if let Ok(message) = serde_json::from_slice(&payload) {
                inspect(&message);
            }
        }
    }
}

pub(crate) fn apply_notification_to_runtime(mux: &Mux, message: &Value) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = message.get("params").unwrap_or(&Value::Null);
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        return;
    };
    let pane_ids: Vec<_> = mux
        .agent_metadata_by_pane
        .read()
        .iter()
        .filter(|(_, metadata)| {
            metadata
                .codex_app_server
                .as_ref()
                .map(|session| session.thread_id.as_str())
                == Some(thread_id)
        })
        .map(|(pane_id, _)| *pane_id)
        .collect();
    for pane_id in pane_ids {
        let mut runtimes = mux.agent_runtime_by_pane.write();
        let Some(runtime) = runtimes.get_mut(&pane_id) else {
            continue;
        };
        runtime.observed_at = Utc::now();
        runtime.transport = AgentTransport::CodexAppServerTui;
        runtime.harness_mode = Some("app-server-tui".to_string());
        match method {
            "turn/started" => {
                if let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) {
                    runtime.turn_state = AgentTurnState::WaitingOnAgent;
                    runtime.turn_phase = Some("running".to_string());
                    runtime.attention_reason = None;
                    runtime.observer_error = None;
                    runtime.observed_turn = Some(AgentObservedTurn {
                        provider_turn_id: turn_id.to_string(),
                        outcome: AgentObservedTurnOutcome::Running,
                        started_at: Some(Utc::now()),
                        completed_at: None,
                        started_cursor: None,
                        latest_cursor: None,
                        primary_user_message_sha256: None,
                        user_message_count: 1,
                        final_message: None,
                    });
                }
            }
            "turn/completed" => {
                let status = params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                let final_message = params
                    .pointer("/turn/items")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items.iter().rev().find(|item| {
                            item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        })
                    })
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                runtime.turn_state = AgentTurnState::WaitingOnUser;
                runtime.turn_phase = Some(status.to_string());
                runtime.last_turn_completed_at = Some(Utc::now());
                runtime.attention_reason = None;
                runtime.observer_error = None;
                if let Some(turn) = runtime.observed_turn.as_mut() {
                    turn.outcome = if status == "completed" {
                        AgentObservedTurnOutcome::Completed
                    } else {
                        AgentObservedTurnOutcome::Aborted
                    };
                    turn.completed_at = Some(Utc::now());
                    turn.final_message = final_message;
                }
            }
            "thread/status/changed" => {
                apply_thread_status(runtime, params.get("status").unwrap_or(&Value::Null));
            }
            "item/started" => {
                runtime.turn_state = AgentTurnState::WaitingOnAgent;
                runtime.attention_reason = None;
                runtime.last_progress_at = Some(Utc::now());
                runtime.progress_summary = params
                    .pointer("/item/type")
                    .and_then(Value::as_str)
                    .map(|kind| format!("Codex {kind}"));
            }
            "item/completed" => {
                runtime.last_progress_at = Some(Utc::now());
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval" => {
                runtime.turn_state = AgentTurnState::WaitingOnUser;
                runtime.attention_reason = Some("approval-requested".to_string());
            }
            "error" => {
                runtime.observer_error = Some(
                    params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex app-server error")
                        .to_string(),
                );
            }
            _ => {}
        }
        finalize_runtime_snapshot(runtime);
        let runtime = runtime.clone();
        drop(runtimes);
        if matches!(method, "turn/started" | "item/completed" | "turn/completed") {
            if let Some(metadata) = mux.agent_metadata_by_pane.read().get(&pane_id).cloned() {
                mux.persist_codex_app_server_notification(
                    (*metadata).clone(),
                    runtime,
                    message.clone(),
                );
            }
        }
        if let Some((_, _, tab_id)) = mux.resolve_pane_id(pane_id) {
            mux.notify_tab_title_changed(tab_id);
        }
    }
}

fn apply_thread_status(runtime: &mut crate::agent::AgentRuntimeSnapshot, status: &Value) {
    match status.get("type").and_then(Value::as_str) {
        Some("active") => {
            runtime.turn_state = AgentTurnState::WaitingOnAgent;
            runtime.attention_reason = None;
            runtime.observer_error = None;
            if matches!(runtime.turn_phase.as_deref(), Some("systemError")) {
                runtime.turn_phase = Some("running".to_string());
            }
        }
        Some("idle") => {
            runtime.turn_state = AgentTurnState::WaitingOnUser;
            runtime.observer_error = None;
            if matches!(runtime.turn_phase.as_deref(), Some("systemError")) {
                runtime.turn_phase = None;
            }
        }
        Some("systemError") => {
            runtime.turn_phase = Some("systemError".to_string());
        }
        _ => {}
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::agent::{AgentHarness, AgentMetadata, AgentRuntimeSnapshot, AgentStatus};
    use crate::agent_event::AgentEventKind;

    fn metadata(name: &str, thread_id: &str) -> AgentMetadata {
        AgentMetadata {
            agent_id: format!("agent-{name}"),
            name: name.to_string(),
            launch_cmd: "codex".to_string(),
            declared_cwd: format!("/tmp/{name}"),
            adopted_pid: None,
            adopted_start_time: None,
            created_at: Utc::now(),
            repo_root: None,
            worktree: None,
            branch: None,
            managed_checkout: false,
            codex_app_server: Some(CodexAppServerSession {
                thread_id: thread_id.to_string(),
                session_id: format!("session-{name}"),
                executable: "/usr/bin/codex".to_string(),
                version: "codex-cli test".to_string(),
                tui_args: vec![],
            }),
        }
    }

    #[test]
    fn opts_into_metadata_only_thread_resume() {
        assert_eq!(initialize_params()["capabilities"]["experimentalApi"], true);
        assert_eq!(
            metadata_only_resume_params("thread-id", "/code/project", &[]),
            json!({
                "threadId": "thread-id",
                "cwd": "/code/project",
                "excludeTurns": true,
                "initialTurnsPage": {
                    "limit": 2,
                    "sortDirection": "desc",
                    "itemsView": "notLoaded"
                }
            })
        );
    }

    #[test]
    fn initial_thread_requests_apply_saved_authority_settings() {
        let args = vec![
            "-a".to_string(),
            "never".to_string(),
            "-s".to_string(),
            "danger-full-access".to_string(),
        ];
        assert_eq!(
            metadata_only_resume_params("thread-id", "/code/project", &args),
            json!({
                "threadId": "thread-id",
                "cwd": "/code/project",
                "excludeTurns": true,
                "initialTurnsPage": {
                    "limit": 2,
                    "sortDirection": "desc",
                    "itemsView": "notLoaded"
                },
                "approvalPolicy": "never",
                "sandbox": "danger-full-access"
            })
        );
        assert_eq!(
            thread_start_params(
                "/code/project",
                &["--dangerously-bypass-approvals-and-sandbox".to_string()]
            ),
            json!({
                "cwd": "/code/project",
                "serviceName": "wakterm",
                "approvalPolicy": "never",
                "sandbox": "danger-full-access"
            })
        );
        assert_eq!(
            metadata_only_resume_params(
                "thread-id",
                "/code/project",
                &[
                    "--ask-for-approval=on-request".to_string(),
                    "--sandbox=workspace-write".to_string(),
                ]
            ),
            json!({
                "threadId": "thread-id",
                "cwd": "/code/project",
                "excludeTurns": true,
                "initialTurnsPage": {
                    "limit": 2,
                    "sortDirection": "desc",
                    "itemsView": "notLoaded"
                },
                "approvalPolicy": "on-request",
                "sandbox": "workspace-write"
            })
        );
    }

    #[test]
    fn validates_only_deliberately_supported_tui_options() {
        validate_tui_args(&[
            "--model".into(),
            "gpt-5.4".into(),
            "--search".into(),
            "--sandbox=workspace-write".into(),
        ])
        .unwrap();
        for args in [
            vec!["--remote".into(), "unix:///tmp/other".into()],
            vec!["--last".into()],
            vec!["--enable=example".into()],
            vec!["prompt text".into()],
            vec!["--model".into()],
        ] {
            assert!(validate_tui_args(&args).is_err(), "accepted {:?}", args);
        }
    }

    #[test]
    fn response_dispatch_uses_exact_request_id() {
        let (writer, _peer) = UnixStream::pair().unwrap();
        let connection = Connection {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(3),
        };
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        connection.pending.lock().insert(1, first_tx);
        connection.pending.lock().insert(2, second_tx);

        connection.dispatch(json!({
            "id": 1,
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "unbound"}
        }));
        assert_eq!(connection.pending.lock().len(), 2);
        connection.dispatch(json!({"id": 2, "result": {"thread": "two"}}));
        connection.dispatch(json!({"id": 1, "result": {"thread": "one"}}));

        assert_eq!(first_rx.recv().unwrap().unwrap()["thread"], "one");
        assert_eq!(second_rx.recv().unwrap().unwrap()["thread"], "two");
        assert!(connection.pending.lock().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn attaches_only_to_an_already_running_exact_thread() {
        let thread_id = "01a02767-c120-77b2-88a1-4e17c93a7549";
        let (writer, peer) = UnixStream::pair().unwrap();
        let connection = Arc::new(Connection {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let responder_connection = Arc::clone(&connection);
        let responder = thread::spawn(move || {
            let mut requests = 0;
            read_messages(peer, |opcode, payload| {
                assert_eq!(opcode, 1);
                let request: Value = serde_json::from_slice(&payload).unwrap();
                let id = request["id"].as_u64().unwrap();
                match request["method"].as_str().unwrap() {
                    "thread/resume" => {
                        assert_eq!(request["params"]["cwd"], "/code/wakterm");
                        assert_eq!(
                            request["params"]["initialTurnsPage"],
                            json!({
                                "limit": 2,
                                "sortDirection": "desc",
                                "itemsView": "notLoaded"
                            })
                        );
                        responder_connection.dispatch(json!({
                            "id": id,
                            "result": {
                                "thread": {
                                    "id": thread_id,
                                    "sessionId": thread_id,
                                    "status": {"type": "idle"}
                                },
                                "initialTurnsPage": {
                                    "data": [
                                        {"id": "running", "completedAt": null},
                                        {"id": "completed", "completedAt": 1_777_000_002}
                                    ],
                                    "nextCursor": null,
                                    "backwardsCursor": null
                                }
                            }
                        }))
                    }
                    "thread/name/set" => {
                        responder_connection.dispatch(json!({"id": id, "result": {}}))
                    }
                    method => panic!("unexpected request {}", method),
                }
                requests += 1;
                requests < 2
            })
            .unwrap();
        });

        let server = CodexAppServer::new(9001);
        let endpoint = {
            let mut state = server.state.lock();
            let endpoint = codex_socket_url(&state.socket_path);
            state.child = Some(Command::new("sleep").arg("60").spawn().unwrap());
            state.connection = Some(connection);
            state.executable = Some("/usr/local/bin/codex".to_string());
            state.version = Some("codex-cli test".to_string());
            endpoint
        };
        let prepared = server
            .attach_existing(
                PrepareCodexLaunch {
                    name: "wakterm_codex".to_string(),
                    cwd: "/code/wakterm".to_string(),
                    resume_thread_id: Some(thread_id.to_string()),
                    tui_args: vec!["--dangerously-bypass-approvals-and-sandbox".to_string()],
                },
                &endpoint,
            )
            .unwrap();
        responder.join().unwrap();

        assert_eq!(prepared.session.thread_id, thread_id);
        assert_eq!(prepared.session.session_id, thread_id);
        assert_eq!(prepared.session.executable, "/usr/local/bin/codex");
        assert_eq!(prepared.session.version, "codex-cli test");
        assert_eq!(
            prepared.argv,
            vec![
                "/usr/local/bin/codex",
                "resume",
                "--remote",
                endpoint.as_str(),
                "-C",
                "/code/wakterm",
                thread_id,
                "--dangerously-bypass-approvals-and-sandbox",
            ]
        );
        let seed = server
            .thread_runtime_seed_by_id
            .lock()
            .get(thread_id)
            .cloned()
            .unwrap();
        assert_eq!(seed.status, json!({"type": "idle"}));
        assert_eq!(
            seed.last_turn_completed_at,
            Utc.timestamp_opt(1_777_000_002, 0).single()
        );
        let mut runtime = AgentRuntimeSnapshot::new(&metadata("attached", thread_id));
        server.prime_runtime(thread_id, &mut runtime);
        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(runtime.last_turn_completed_at, seed.last_turn_completed_at);
    }

    #[test]
    fn live_completion_advances_the_cached_runtime_seed() {
        let server = CodexAppServer::new(9002);
        let thread_id = "thread-live-completion";
        server.record_thread_bootstrap(&json!({
            "thread": {
                "id": thread_id,
                "status": {"type": "active"}
            },
            "initialTurnsPage": {
                "data": [{"id": "previous", "completedAt": 1_777_000_002}]
            }
        }));
        server.record_notification(&json!({
            "method": "turn/completed",
            "params": {
                "threadId": thread_id,
                "turn": {"id": "latest", "completedAt": 1_777_000_010}
            }
        }));
        server.record_notification(&json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": thread_id,
                "status": {"type": "idle"}
            }
        }));

        let mut runtime = AgentRuntimeSnapshot::new(&metadata("live", thread_id));
        server.prime_runtime(thread_id, &mut runtime);

        assert_eq!(runtime.turn_state, AgentTurnState::WaitingOnUser);
        assert_eq!(
            runtime.last_turn_completed_at,
            Utc.timestamp_opt(1_777_000_010, 0).single()
        );
    }

    #[test]
    fn notifications_update_only_the_exact_thread() {
        let mux = Mux::new(None);
        let first = metadata("first", "thread-a");
        let second = metadata("second", "thread-b");
        mux.agent_metadata_by_pane
            .write()
            .insert(1, Arc::new(first.clone()));
        mux.agent_metadata_by_pane
            .write()
            .insert(2, Arc::new(second.clone()));
        mux.agent_runtime_by_pane
            .write()
            .insert(1, AgentRuntimeSnapshot::new(&first));
        mux.agent_runtime_by_pane
            .write()
            .insert(2, AgentRuntimeSnapshot::new(&second));

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "turn/started",
                "params": {"threadId": "thread-b", "turn": {"id": "turn-b"}}
            }),
        );

        let runtimes = mux.agent_runtime_by_pane.read();
        assert!(runtimes[&1].observed_turn.is_none());
        assert_eq!(
            runtimes[&2]
                .observed_turn
                .as_ref()
                .map(|turn| turn.provider_turn_id.as_str()),
            Some("turn-b")
        );
        drop(runtimes);
        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "turn/completed",
                "params": {"threadId": "thread-b", "turn": {"id": "turn-b", "status": "interrupted"}}
            }),
        );
        {
            let runtimes = mux.agent_runtime_by_pane.read();
            assert_eq!(runtimes[&2].status, AgentStatus::Idle);
            assert_eq!(runtimes[&2].turn_state, AgentTurnState::WaitingOnUser);
            assert_eq!(
                runtimes[&2].attention_reason.as_deref(),
                Some("turn-aborted")
            );
        }
        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "turn/started",
                "params": {"threadId": "thread-b", "turn": {"id": "turn-after-cancel"}}
            }),
        );
        let runtimes = mux.agent_runtime_by_pane.read();
        assert_eq!(
            runtimes[&2]
                .observed_turn
                .as_ref()
                .map(|turn| turn.provider_turn_id.as_str()),
            Some("turn-after-cancel")
        );
        assert_eq!(runtimes[&2].status, AgentStatus::Busy);
        drop(runtimes);

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "item/commandExecution/requestApproval",
                "params": {"threadId": "thread-b"}
            }),
        );
        {
            let runtimes = mux.agent_runtime_by_pane.read();
            assert_eq!(runtimes[&2].status, AgentStatus::Busy);
            assert_eq!(runtimes[&2].turn_state, AgentTurnState::WaitingOnUser);
            assert_eq!(
                runtimes[&2].attention_reason.as_deref(),
                Some("approval-requested")
            );
        }

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "item/started",
                "params": {"threadId": "thread-b", "item": {"type": "commandExecution"}}
            }),
        );
        {
            let runtimes = mux.agent_runtime_by_pane.read();
            assert_eq!(runtimes[&2].status, AgentStatus::Busy);
            assert_eq!(runtimes[&2].turn_state, AgentTurnState::WaitingOnAgent);
            assert_eq!(runtimes[&2].attention_reason, None);
        }

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "thread/status/changed",
                "params": {"threadId": "thread-b", "status": {"type": "systemError"}}
            }),
        );
        {
            let runtimes = mux.agent_runtime_by_pane.read();
            assert_eq!(runtimes[&2].status, AgentStatus::Errored);
            assert_eq!(
                runtimes[&2].attention_reason.as_deref(),
                Some("system-error")
            );
        }

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "thread/status/changed",
                "params": {"threadId": "thread-b", "status": {"type": "active"}}
            }),
        );
        let runtimes = mux.agent_runtime_by_pane.read();
        assert_eq!(runtimes[&2].status, AgentStatus::Busy);
        assert_eq!(runtimes[&2].turn_state, AgentTurnState::WaitingOnAgent);
        assert_eq!(runtimes[&2].attention_reason, None);
    }

    #[test]
    fn restored_app_server_notifications_produce_durable_agent_events() {
        let mux = Mux::new(None);
        mux.start_agent_event_runtime_epoch().unwrap();
        let metadata = metadata("restored", "thread-restored");
        assert!(metadata.adopted_pid.is_none());
        assert!(metadata.adopted_start_time.is_none());
        assert!(crate::agent_admission::incarnation_id(&metadata).is_some());
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = crate::agent::AgentHarness::Codex;
        runtime.alive = true;
        runtime.session_path = None;
        mux.agent_metadata_by_pane
            .write()
            .insert(7, Arc::new(metadata));
        mux.agent_runtime_by_pane.write().insert(7, runtime);

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "turn/started",
                "params": {
                    "threadId": "thread-restored",
                    "turn": {"id": "turn-restored", "startedAt": 1_777_000_000}
                }
            }),
        );
        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-restored",
                    "turnId": "turn-restored",
                    "completedAtMs": 1_777_000_001_000_i64,
                    "item": {
                        "id": "message-restored",
                        "type": "agentMessage",
                        "text": "restored answer"
                    }
                }
            }),
        );
        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-restored",
                    "turn": {
                        "id": "turn-restored",
                        "status": "completed",
                        "completedAt": 1_777_000_002,
                        "items": [{
                            "id": "message-restored",
                            "type": "agentMessage",
                            "text": "restored answer"
                        }]
                    }
                }
            }),
        );

        let page = (0..100)
            .find_map(|_| {
                let page = mux.agent_event_store.read_page(0, 100).unwrap();
                if page
                    .events
                    .iter()
                    .any(|event| event.kind == crate::agent_event::AgentEventKind::TurnFinal)
                {
                    Some(page)
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("app-server final was not persisted");
        let message = page
            .events
            .iter()
            .find(|event| event.kind == crate::agent_event::AgentEventKind::AssistantMessage)
            .expect("assistant message event");
        assert_eq!(message.turn_id.as_deref(), Some("turn-restored"));
        assert_eq!(message.text.as_deref(), Some("restored answer"));
        let final_event = page
            .events
            .iter()
            .find(|event| event.kind == crate::agent_event::AgentEventKind::TurnFinal)
            .expect("turn final event");
        assert_eq!(final_event.turn_id.as_deref(), Some("turn-restored"));
        assert_eq!(final_event.outcome.as_deref(), Some("completed"));
        assert_eq!(final_event.text.as_deref(), Some("restored answer"));

        apply_notification_to_runtime(
            &mux,
            &json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-restored",
                    "turnId": "turn-restored",
                    "completedAtMs": 1_777_000_001_000_i64,
                    "item": {
                        "id": "message-restored",
                        "type": "agentMessage",
                        "text": "restored answer"
                    }
                }
            }),
        );
        std::thread::sleep(Duration::from_millis(20));
        let repeated = mux.agent_event_store.read_page(0, 100).unwrap();
        assert_eq!(
            repeated
                .events
                .iter()
                .filter(|event| event.kind == crate::agent_event::AgentEventKind::AssistantMessage)
                .count(),
            1
        );
    }

    #[test]
    fn tui_proxy_correlates_a_successful_resume_with_its_connection() {
        let mut state = TuiProxyProtocolState::new("thread-a");
        state.record_client_message(&json!({
            "id": 41,
            "method": "thread/resume",
            "params": {"threadId": "thread-b"}
        }));
        assert!(state
            .record_server_message(&json!({
                "method": "turn/started",
                "params": {"threadId": "thread-b", "turn": {"id": "turn-b"}}
            }))
            .notifications
            .is_empty());

        let dispatch = state.record_server_message(&json!({
            "id": 41,
            "result": {
                "thread": {
                    "id": "thread-b",
                    "sessionId": "session-b",
                    "status": {"type": "active"}
                }
            }
        }));
        let transition = dispatch.transition.expect("thread transition");
        assert_eq!(transition.old_thread_id, "thread-a");
        assert_eq!(transition.new_thread_id, "thread-b");
        assert_eq!(transition.session_id, "session-b");
        assert_eq!(dispatch.notifications.len(), 1);

        let next = state.record_server_message(&json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-b",
                "turnId": "turn-b",
                "item": {"id": "message-b", "type": "agentMessage", "text": "answer b"}
            }
        }));
        assert!(next.transition.is_none());
        assert_eq!(next.notifications.len(), 1);
        assert!(state
            .record_server_message(&json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-a",
                    "turnId": "turn-a",
                    "item": {"id": "late-a", "type": "agentMessage", "text": "late a"}
                }
            }))
            .notifications
            .is_empty());
    }

    #[test]
    fn tui_proxy_does_not_follow_an_ephemeral_thread_start() {
        let mut state = TuiProxyProtocolState::new("thread-primary");
        state.record_client_message(&json!({
            "id": "temporary-structured-title",
            "method": "thread/start",
            "params": {
                "ephemeral": true,
                "threadSource": {"type": "feature", "name": "system"}
            }
        }));

        let dispatch = state.record_server_message(&json!({
            "id": "temporary-structured-title",
            "result": {
                "thread": {
                    "id": "thread-title-generator",
                    "sessionId": "thread-title-generator",
                    "status": {"type": "active"}
                }
            }
        }));
        assert!(dispatch.transition.is_none());
        assert!(dispatch.notifications.is_empty());
        assert!(state
            .record_server_message(&json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-title-generator",
                    "turn": {"id": "turn-title", "status": "completed", "items": []}
                }
            }))
            .notifications
            .is_empty());

        state.record_client_message(&json!({
            "id": 42,
            "method": "thread/resume",
            "params": {"threadId": "thread-visible"}
        }));
        let transition = state
            .record_server_message(&json!({
                "id": 42,
                "result": {
                    "thread": {
                        "id": "thread-visible",
                        "sessionId": "session-visible",
                        "status": {"type": "idle"}
                    }
                }
            }))
            .transition
            .expect("visible resume transition");
        assert_eq!(transition.old_thread_id, "thread-primary");
        assert_eq!(transition.new_thread_id, "thread-visible");
    }

    #[test]
    fn durable_output_follows_the_managed_pane_to_a_resumed_thread() {
        let mux = Mux::new(None);
        mux.start_agent_event_runtime_epoch().unwrap();
        let metadata = metadata("switching", "thread-a");
        let agent_id = metadata.agent_id.clone();
        let mut runtime = AgentRuntimeSnapshot::new(&metadata);
        runtime.harness = AgentHarness::Codex;
        runtime.alive = true;
        mux.agent_metadata_by_pane
            .write()
            .insert(23, Arc::new(metadata));
        mux.agent_runtime_by_pane.write().insert(23, runtime);

        for message in [
            json!({
                "method": "turn/started",
                "params": {"threadId": "thread-a", "turn": {"id": "turn-a"}}
            }),
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-a",
                    "turnId": "turn-a",
                    "item": {"id": "message-a", "type": "agentMessage", "text": "answer a"}
                }
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-a",
                    "turn": {
                        "id": "turn-a",
                        "status": "completed",
                        "items": [{"id": "message-a", "type": "agentMessage", "text": "answer a"}]
                    }
                }
            }),
        ] {
            apply_notification_to_runtime(&mux, &message);
        }

        let mut proxy = TuiProxyProtocolState::new("thread-a");
        proxy.record_client_message(&json!({
            "id": 42,
            "method": "thread/resume",
            "params": {"threadId": "thread-b"}
        }));
        let early_turn_started = json!({
            "method": "turn/started",
            "params": {"threadId": "thread-b", "turn": {"id": "turn-b"}}
        });
        assert!(proxy
            .record_server_message(&early_turn_started)
            .notifications
            .is_empty());
        apply_tui_proxy_dispatch(
            &mux,
            proxy.record_server_message(&json!({
                "id": 42,
                "result": {
                    "thread": {
                        "id": "thread-b",
                        "sessionId": "session-b",
                        "status": {"type": "active"}
                    }
                }
            })),
        );
        for message in [
            json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thread-b",
                    "turnId": "turn-b",
                    "item": {"id": "message-b", "type": "agentMessage", "text": "answer b"}
                }
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": "thread-b",
                    "turn": {
                        "id": "turn-b",
                        "status": "completed",
                        "items": [{"id": "message-b", "type": "agentMessage", "text": "answer b"}]
                    }
                }
            }),
        ] {
            apply_tui_proxy_dispatch(&mux, proxy.record_server_message(&message));
        }

        let page = (0..100)
            .find_map(|_| {
                let page = mux.agent_event_store.read_page(0, 100).unwrap();
                let messages = page
                    .events
                    .iter()
                    .filter(|event| event.kind == AgentEventKind::AssistantMessage)
                    .count();
                if messages == 2 {
                    Some(page)
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("both pane-scoped messages were not persisted");
        let messages = page
            .events
            .iter()
            .filter(|event| event.kind == AgentEventKind::AssistantMessage)
            .collect::<Vec<_>>();
        assert_eq!(messages[0].agent_id, agent_id);
        assert_eq!(messages[0].text.as_deref(), Some("answer a"));
        assert_eq!(messages[1].agent_id, agent_id);
        assert_eq!(messages[1].text.as_deref(), Some("answer b"));
        assert_ne!(messages[0].incarnation_id, messages[1].incarnation_id);
        let current = mux.agent_metadata_by_pane.read()[&23].clone();
        let current_session = current.codex_app_server.as_ref().unwrap();
        assert_eq!(current_session.thread_id, "thread-b");
        assert_eq!(current_session.session_id, "session-b");
    }

    #[cfg(unix)]
    #[test]
    fn native_tui_uses_the_exact_process_on_unix() {
        let native = vec!["codex".to_string(), "resume".to_string()];
        assert_eq!(native_tui_argv(&native), native);
    }

    #[cfg(windows)]
    #[test]
    fn windows_socket_urls_and_tui_launch_are_native() {
        assert_eq!(
            codex_socket_url(Path::new(r"C:\Users\Mihai\wakterm.sock")),
            "unix://C:/Users/Mihai/wakterm.sock"
        );
        assert_eq!(powershell_quote("a'b c"), "'a''b c'");
        let argv = native_tui_argv(&[
            r"C:\Program Files\Codex\codex.exe".to_string(),
            "resume".to_string(),
            "thread-id".to_string(),
        ]);
        assert_eq!(argv.first().map(String::as_str), Some("powershell.exe"));
        assert!(argv.iter().any(|arg| arg == "-EncodedCommand"));
    }

    #[test]
    #[ignore = "requires the installed Codex app-server"]
    fn real_installed_codex_resumes_existing_thread_without_history() {
        let thread_id = std::env::var("WAKTERM_TEST_CODEX_THREAD_ID")
            .expect("WAKTERM_TEST_CODEX_THREAD_ID must name an existing thread");
        let cwd = std::env::var("WAKTERM_TEST_CODEX_CWD")
            .expect("WAKTERM_TEST_CODEX_CWD must name the thread working directory");
        let server = CodexAppServer::new(usize::MAX - 1);

        let resumed = server
            .prepare(PrepareCodexLaunch {
                name: "wakterm-real-resume-smoke".to_string(),
                cwd,
                resume_thread_id: Some(thread_id.clone()),
                tui_args: vec![],
            })
            .unwrap();

        assert_eq!(resumed.session.thread_id, thread_id);
        assert!(!resumed.session.session_id.is_empty());
    }

    #[test]
    #[ignore = "requires the installed Codex app-server"]
    fn real_installed_codex_accepts_authority_settings() {
        let root = tempfile::tempdir().unwrap();
        let server = CodexAppServer::new(usize::MAX - 2);
        let prepared = server
            .prepare(PrepareCodexLaunch {
                name: "wakterm-real-authority-smoke".to_string(),
                cwd: root.path().to_string_lossy().to_string(),
                resume_thread_id: None,
                tui_args: vec![
                    "-a".to_string(),
                    "never".to_string(),
                    "-s".to_string(),
                    "danger-full-access".to_string(),
                ],
            })
            .unwrap();
        assert!(!prepared.session.thread_id.is_empty());

        let mut state = server.state.lock();
        let child = state.child.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[ignore = "requires the installed Codex app-server"]
    fn real_installed_codex_uses_one_server_and_resumes_exact_threads() {
        let root = tempfile::tempdir().unwrap();
        let first_cwd = root.path().join("first");
        let second_cwd = root.path().join("second");
        std::fs::create_dir_all(&first_cwd).unwrap();
        std::fs::create_dir_all(&second_cwd).unwrap();
        let server = CodexAppServer::new(usize::MAX);
        let first = server
            .prepare(PrepareCodexLaunch {
                name: "wakterm-real-smoke-first".to_string(),
                cwd: first_cwd.to_string_lossy().to_string(),
                resume_thread_id: None,
                tui_args: vec!["--no-alt-screen".to_string()],
            })
            .unwrap();
        let first_pid = server.state.lock().child.as_ref().unwrap().id();
        let second = server
            .prepare(PrepareCodexLaunch {
                name: "wakterm-real-smoke-second".to_string(),
                cwd: second_cwd.to_string_lossy().to_string(),
                resume_thread_id: None,
                tui_args: vec![],
            })
            .unwrap();
        assert_ne!(first.session.thread_id, second.session.thread_id);
        assert_eq!(server.state.lock().child.as_ref().unwrap().id(), first_pid);
        let resumed = server
            .prepare(PrepareCodexLaunch {
                name: "wakterm-real-smoke-first".to_string(),
                cwd: first_cwd.to_string_lossy().to_string(),
                resume_thread_id: Some(first.session.thread_id.clone()),
                tui_args: vec![],
            })
            .unwrap();
        assert_eq!(resumed.session.thread_id, first.session.thread_id);
        assert_eq!(resumed.session.session_id, first.session.session_id);

        {
            let mut state = server.state.lock();
            let child = state.child.as_mut().unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
        }
        server
            .recover(&[
                RecoveryThread {
                    name: "wakterm-real-smoke-first".to_string(),
                    cwd: first_cwd.to_string_lossy().to_string(),
                    session: first.session.clone(),
                },
                RecoveryThread {
                    name: "wakterm-real-smoke-second".to_string(),
                    cwd: second_cwd.to_string_lossy().to_string(),
                    session: second.session.clone(),
                },
            ])
            .unwrap();
        assert_ne!(server.state.lock().child.as_ref().unwrap().id(), first_pid);
        assert!(server.recover(&[]).is_err());

        let state = server.state.lock();
        let connection = state.connection.as_ref().unwrap();
        for thread_id in [first.session.thread_id, second.session.thread_id] {
            connection
                .request("thread/archive", json!({"threadId": thread_id}))
                .unwrap();
        }
    }
}
