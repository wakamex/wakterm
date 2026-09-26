//! Automatic shell integration for local panes.
//!
//! Wakterm loads its shell integration into bash, zsh and fish without
//! changes to the user's startup files. Each shell gets a startup hook that
//! first restores the user's own environment and startup behavior:
//!
//! * zsh, and Wsh, which is built on it, read `.zshenv` from `ZDOTDIR`,
//!   which points at Wakterm's directory.
//! * bash runs as `bash --posix`, which reads `ENV` before any startup file;
//!   that script turns POSIX mode back off and runs the usual startup files.
//! * fish reads `vendor_conf.d` from each `XDG_DATA_DIRS` entry.
//!
//! The scripts are embedded in the binary and written once per mux process
//! to a content-addressed directory under the runtime directory.

use portable_pty::CommandBuilder;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const FILES: &[(&str, &str)] = &[
    (
        "wakterm.sh",
        include_str!("../../assets/shell-integration/wakterm.sh"),
    ),
    (
        "zsh/.zshenv",
        include_str!("../../assets/shell-integration-inject/zsh/.zshenv"),
    ),
    (
        "bash/inject.bash",
        include_str!("../../assets/shell-integration-inject/bash/inject.bash"),
    ),
    (
        "fish/fish/vendor_conf.d/wakterm.fish",
        include_str!("../../assets/shell-integration-inject/fish/fish/vendor_conf.d/wakterm.fish"),
    ),
];

/// Exported to injected shells; the startup hooks load scripts from it.
const DIR_ENV: &str = "WAKTERM_SHELL_INTEGRATION_DIR";

fn integration_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(
        || match write_files(&config::RUNTIME_DIR.join("shell-integration")) {
            Ok(dir) => Some(dir),
            Err(err) => {
                log::error!("shell integration is unavailable: {err:#}");
                None
            }
        },
    )
    .as_deref()
}

fn write_files(root: &Path) -> anyhow::Result<PathBuf> {
    let mut digest = Sha256::new();
    for (name, contents) in FILES {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(contents.as_bytes());
        digest.update([0]);
    }
    let dir = root.join(&format!("{:x}", digest.finalize())[..16]);
    if dir.is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(root)?;
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(root)?;
    for (name, contents) in FILES {
        let path = staging.path().join(name);
        std::fs::create_dir_all(path.parent().expect("file has a parent"))?;
        std::fs::write(&path, contents)?;
    }
    // Another mux process may publish the same contents first.
    match std::fs::rename(staging.path(), &dir) {
        Ok(()) => {
            let _ = staging.keep();
        }
        Err(_) if dir.is_dir() => {}
        Err(err) => return Err(err.into()),
    }
    Ok(dir)
}

/// Arrange for the shell that `cmd` starts to load Wakterm's integration.
/// Commands that are not a recognized shell invocation are left unchanged.
pub fn inject(cmd: &mut CommandBuilder) {
    let shell = if cmd.is_default_prog() {
        OsString::from(cmd.get_shell())
    } else {
        cmd.get_argv()[0].clone()
    };
    let name = Path::new(&shell)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .trim_start_matches('-')
        .to_string();
    let applies = match name.as_str() {
        // Wsh reads zsh's startup files and owns the features it implements
        // natively, so only Wakterm's own additions load there.
        "zsh" | "wsh" | "fish" => true,
        "bash" => bash_posix_argv(cmd, &shell).is_some(),
        _ => false,
    };
    if !applies {
        return;
    }
    let Some(dir) = integration_dir() else {
        return;
    };
    inject_with_dir(cmd, &shell, &name, dir);
}

fn inject_with_dir(cmd: &mut CommandBuilder, shell: &OsStr, name: &str, dir: &Path) {
    match name {
        "zsh" | "wsh" => {
            if let Some(zdotdir) = cmd.get_env("ZDOTDIR").map(OsStr::to_owned) {
                cmd.env("WAKTERM_ORIG_ZDOTDIR", zdotdir);
            }
            cmd.env("ZDOTDIR", dir.join("zsh"));
        }
        "bash" => {
            let Some((argv, login)) = bash_posix_argv(cmd, shell) else {
                return;
            };
            if cmd.is_default_prog() {
                cmd.replace_default_prog(argv);
            } else {
                *cmd.get_argv_mut() = argv;
            }
            if login {
                cmd.env("WAKTERM_BASH_LOGIN", "1");
            }
            if let Some(env) = cmd.get_env("ENV").map(OsStr::to_owned) {
                cmd.env("WAKTERM_BASH_ORIG_ENV", env);
            }
            cmd.env("ENV", dir.join("bash/inject.bash"));
            // POSIX mode would otherwise default HISTFILE to ~/.sh_history.
            if cmd.get_env("HISTFILE").is_none() {
                if let Some(home) = cmd.get_env("HOME").map(PathBuf::from) {
                    cmd.env("HISTFILE", home.join(".bash_history"));
                    cmd.env("WAKTERM_BASH_UNEXPORT_HISTFILE", "1");
                }
            }
        }
        "fish" => {
            let original = cmd.get_env("XDG_DATA_DIRS").map(OsStr::to_owned);
            let mut dirs = dir.join("fish").into_os_string();
            dirs.push(":");
            match &original {
                Some(original) => {
                    cmd.env("WAKTERM_FISH_ORIG_XDG_DATA_DIRS", original);
                    dirs.push(original);
                }
                // The XDG default, which setting the variable replaces.
                None => dirs.push("/usr/local/share:/usr/share"),
            }
            cmd.env("XDG_DATA_DIRS", dirs);
        }
        _ => return,
    }
    cmd.env(DIR_ENV, dir);
}

/// The `bash --posix` command line for a bash invocation Wakterm can inject
/// into, and whether it was a login shell. Only the default program and
/// invocations made of `-l`, `--login` and `-i`, optionally followed by
/// `-c` and its operands, qualify.
fn bash_posix_argv(cmd: &CommandBuilder, shell: &OsStr) -> Option<(Vec<OsString>, bool)> {
    let mut argv = vec![shell.to_owned(), "--posix".into()];
    if cmd.is_default_prog() {
        return Some((argv, true));
    }
    let mut login = false;
    let mut interactive = false;
    let mut rest = cmd.get_argv()[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.to_str()? {
            "--login" => login = true,
            "-c" => {
                if interactive {
                    argv.push("-i".into());
                }
                argv.push("-c".into());
                argv.extend(rest.cloned());
                return Some((argv, login));
            }
            flags if flags.starts_with('-') && flags.len() > 1 && !flags.starts_with("--") => {
                for flag in flags[1..].chars() {
                    match flag {
                        'l' => login = true,
                        'i' => interactive = true,
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
    }
    if interactive {
        argv.push("-i".into());
    }
    Some((argv, login))
}

#[cfg(test)]
mod test {
    use super::*;

    fn argv(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env(cmd: &CommandBuilder, key: &str) -> Option<String> {
        cmd.get_env(key)
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn bash_invocations_become_posix_with_login_emulated() {
        let dir = Path::new("/run/wakterm/shell-integration/x");
        let cases: &[(&[&str], Option<(&[&str], bool)>)] = &[
            (&["bash"], Some((&["bash", "--posix"], false))),
            (
                &["/bin/bash", "-l"],
                Some((&["/bin/bash", "--posix"], true)),
            ),
            (
                &["bash", "--login", "-i"],
                Some((&["bash", "--posix", "-i"], true)),
            ),
            (
                &["bash", "-l", "-i", "-c", "claude; exec \"$0\" -l", "bash"],
                Some((
                    &[
                        "bash",
                        "--posix",
                        "-i",
                        "-c",
                        "claude; exec \"$0\" -l",
                        "bash",
                    ],
                    true,
                )),
            ),
            (&["bash", "script.sh"], None),
            (&["bash", "--norc"], None),
            (&["bash", "-x"], None),
        ];
        for (input, expected) in cases {
            let mut cmd = CommandBuilder::from_argv(input.iter().map(OsString::from).collect());
            cmd.env_remove("HISTFILE");
            cmd.env("HOME", "/home/u");
            let shell = cmd.get_argv()[0].clone();
            assert_eq!(
                bash_posix_argv(&cmd, &shell).map(|(argv, login)| (
                    argv.iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                    login
                )),
                expected
                    .map(|(argv, login)| (argv.iter().map(|arg| arg.to_string()).collect(), login)),
                "{input:?}"
            );
            if expected.is_some() {
                inject_with_dir(&mut cmd, &shell, "bash", dir);
                assert_eq!(
                    env(&cmd, "ENV").as_deref(),
                    Some("/run/wakterm/shell-integration/x/bash/inject.bash")
                );
                assert_eq!(
                    env(&cmd, "HISTFILE").as_deref(),
                    Some("/home/u/.bash_history")
                );
                assert_eq!(
                    env(&cmd, "WAKTERM_BASH_LOGIN").is_some(),
                    expected.unwrap().1,
                    "{input:?}"
                );
            }
        }
    }

    #[test]
    fn default_bash_program_is_an_emulated_login_shell() {
        let mut cmd = CommandBuilder::new_default_prog();
        let shell = OsString::from("/usr/bin/bash");
        inject_with_dir(&mut cmd, &shell, "bash", Path::new("/d"));
        assert_eq!(argv(&cmd), ["/usr/bin/bash", "--posix"]);
        assert_eq!(env(&cmd, "WAKTERM_BASH_LOGIN").as_deref(), Some("1"));
    }

    #[test]
    fn zsh_and_fish_keep_the_users_environment_for_restoration() {
        let dir = Path::new("/d");
        let mut zsh = CommandBuilder::new_default_prog();
        zsh.env("ZDOTDIR", "/home/u/.config/zsh");
        inject_with_dir(&mut zsh, OsStr::new("/bin/zsh"), "zsh", dir);
        assert_eq!(env(&zsh, "ZDOTDIR").as_deref(), Some("/d/zsh"));
        assert_eq!(
            env(&zsh, "WAKTERM_ORIG_ZDOTDIR").as_deref(),
            Some("/home/u/.config/zsh")
        );
        assert!(zsh.is_default_prog());

        let mut fish = CommandBuilder::from_argv(vec!["fish".into()]);
        fish.env_remove("XDG_DATA_DIRS");
        inject_with_dir(&mut fish, OsStr::new("fish"), "fish", dir);
        assert_eq!(
            env(&fish, "XDG_DATA_DIRS").as_deref(),
            Some("/d/fish:/usr/local/share:/usr/share")
        );
        assert_eq!(env(&fish, "WAKTERM_FISH_ORIG_XDG_DATA_DIRS"), None);
        assert_eq!(env(&fish, DIR_ENV).as_deref(), Some("/d"));
    }

    #[test]
    fn written_files_match_the_embedded_scripts() {
        let temp = tempfile::tempdir().unwrap();
        let dir = write_files(temp.path()).unwrap();
        assert_eq!(write_files(temp.path()).unwrap(), dir);
        for (name, contents) in FILES {
            assert_eq!(&std::fs::read_to_string(dir.join(name)).unwrap(), contents);
        }
    }
}
