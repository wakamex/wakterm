//! A journal of the Claude Code hook events that mark a turn's end, a wait on
//! the user and a session's start and end, recorded for each Claude that
//! Wakterm starts.
//!
//! Wakterm passes `settings_path()` to Claude with `--settings`. Its hooks
//! run `wakterm record-claude-hook`, which appends the event to the journal
//! of the pane it runs in, named by the pane's durable `WAKTERM_PANE_TOKEN`,
//! so the journal follows the pane across mux restarts and `/clear`. The
//! recorder prints nothing, since a hook's output can reach Claude, and
//! always succeeds, so that a broken journal never changes what Claude does.
//! docs/claude-turn-records.md lists what each event records.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The hook events recorded: `Stop` and `StopFailure` end a turn,
/// `PermissionRequest` marks a wait on the user, and `SessionStart` and
/// `SessionEnd` name the transcript file and prove the journal works.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "Stop",
    "StopFailure",
    "PermissionRequest",
];

/// How long Claude waits for the recorder before going on without it.
const HOOK_TIMEOUT_SECONDS: u64 = 5;

/// A journal larger than this moves aside to `<token>.jsonl.1`, replacing
/// the previous one, before the next event is written.
const JOURNAL_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

pub fn journal_dir() -> PathBuf {
    config::DATA_DIR.join("claude-hooks")
}

/// The settings file Wakterm passes to the Claudes it starts.
pub fn settings_path() -> PathBuf {
    journal_dir().join("settings.json")
}

/// Whether `value`, an argument of `--settings`, is Wakterm's settings file,
/// which a restore passes again and a saved launch command must not keep.
pub fn is_settings_path(value: &str) -> bool {
    Path::new(value) == settings_path()
}

/// The settings registering a hook for each of `EVENTS` that runs `recorder`.
pub fn settings(recorder: &Path) -> serde_json::Value {
    let command = format!(
        "{} record-claude-hook",
        shell_words::quote(&recorder.to_string_lossy())
    );
    let hook = serde_json::json!([{
        "hooks": [{"type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECONDS}]
    }]);
    serde_json::json!({
        "hooks": EVENTS
            .iter()
            .map(|event| (event.to_string(), hook.clone()))
            .collect::<serde_json::Map<_, _>>()
    })
}

static INSTALLED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Writes the settings file for this mux server, as `write_settings` does,
/// so that `installed_settings` names it to the Claudes the server starts.
pub fn install(server: &Path) -> anyhow::Result<PathBuf> {
    let path = write_settings(server)?;
    let _ = INSTALLED.set(path.clone());
    Ok(path)
}

/// The settings file to pass to a Claude this process starts, once
/// `install` has written it.
pub fn installed_settings() -> Option<&'static Path> {
    INSTALLED.get().map(PathBuf::as_path)
}

/// Writes the settings file for the `wakterm` program beside `server`, the
/// mux server's own executable, returning its path.
pub fn write_settings(server: &Path) -> anyhow::Result<PathBuf> {
    let recorder = server.with_file_name(if cfg!(windows) {
        "wakterm.exe"
    } else {
        "wakterm"
    });
    anyhow::ensure!(recorder.is_file(), "{} is missing", recorder.display());
    let dir = journal_dir();
    create_private_dir(&dir)?;
    let path = settings_path();
    let temporary = dir.join("settings.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&settings(&recorder))?)?;
    std::fs::rename(&temporary, &path)?;
    Ok(path)
}

/// Records the hook event on standard input in the journal of the pane named
/// by `WAKTERM_PANE_TOKEN`, with the time it arrived as `received_at`.
/// Nothing is recorded outside a Wakterm pane. Errors are returned for the
/// caller to drop: the recorder must not fail a hook.
pub fn record_from_stdin() -> anyhow::Result<()> {
    let Some(token) = std::env::var_os("WAKTERM_PANE_TOKEN") else {
        return Ok(());
    };
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    record(&journal_dir(), &token.to_string_lossy(), &input)
}

fn record(dir: &Path, token: &str, input: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "pane token {token:?} is not a file name"
    );
    let mut event: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(input)?;
    event.insert(
        "received_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    create_private_dir(dir)?;
    let path = dir.join(format!("{token}.jsonl"));
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > JOURNAL_LIMIT_BYTES) {
        std::fs::rename(&path, path.with_extension("jsonl.1"))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // One write, so that events from hooks running at once never interleave.
    options.open(&path)?.write_all(&line)?;
    Ok(())
}

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_event_becomes_one_private_line_of_its_panes_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        record(
            &journal,
            "pane-1",
            br#"{"hook_event_name":"Stop","prompt_id":"p"}"#,
        )
        .unwrap();
        record(&journal, "pane-1", br#"{"hook_event_name":"SessionEnd"}"#).unwrap();
        let text = std::fs::read_to_string(journal.join("pane-1.jsonl")).unwrap();
        let events = text
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["prompt_id"], "p");
        assert!(events[1]["received_at"].is_string());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&journal), 0o700);
            assert_eq!(mode(&journal.join("pane-1.jsonl")), 0o600);
        }
    }

    #[test]
    fn a_bad_token_or_event_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(record(dir.path(), "../escape", b"{}").is_err());
        assert!(record(dir.path(), "pane-1", b"not json").is_err());
        assert!(!dir.path().join("pane-1.jsonl").exists());
    }

    #[test]
    fn settings_run_the_recorder_for_each_event_with_a_timeout() {
        let settings = settings(Path::new("/opt/wak term/wakterm"));
        let hooks = settings["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), EVENTS.len());
        let hook = &hooks["Stop"][0]["hooks"][0];
        assert_eq!(
            hook["command"],
            "'/opt/wak term/wakterm' record-claude-hook"
        );
        assert_eq!(hook["timeout"], HOOK_TIMEOUT_SECONDS);
    }
}
