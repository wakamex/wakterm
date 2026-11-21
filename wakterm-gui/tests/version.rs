//! `wakterm-gui --version` prints this build's version.

// A Windows GUI-subsystem program has no console to print to unless it is
// asked to attach to its parent's.
#[cfg(not(windows))]
#[test]
fn version_flag_prints_the_build_version() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_wakterm-gui"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let version = stdout.trim().strip_prefix("wakterm-gui ").unwrap();
    assert!(
        !version.is_empty() && !version.contains("assign_version_info"),
        "{:?}",
        stdout
    );
}
