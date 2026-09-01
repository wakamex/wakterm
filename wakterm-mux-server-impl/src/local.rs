use crate::sessionhandler::ConnectionAuthority;
use anyhow::{anyhow, Context as _};
use config::{create_user_owned_dirs, UnixDomain};
#[cfg(target_os = "linux")]
use std::convert::TryFrom;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use wakterm_uds::UnixListener;

pub struct LocalListener {
    listener: UnixListener,
}

impl LocalListener {
    pub fn new(listener: UnixListener) -> Self {
        Self { listener }
    }

    pub fn with_domain(unix_dom: &UnixDomain) -> anyhow::Result<Self> {
        let listener = safely_create_sock_path(unix_dom)?;
        Ok(Self::new(listener))
    }

    pub fn run(&mut self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    let authority = local_connection_authority(&stream);
                    let _ = std::thread::spawn(move || {
                        promise::spawn::block_on(async move {
                            crate::dispatch::process_with_authority(stream, authority)
                                .await
                                .map_err(|e| {
                                    log::error!("{:#}", e);
                                    e
                                })
                        })
                        .ok();
                    });
                }
                Err(err) => {
                    log::error!("accept failed: {}", err);
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NamespaceIdentity {
    user: (u64, u64),
    mount: (u64, u64),
    pid: (u64, u64),
    ipc: (u64, u64),
}

#[cfg(target_os = "linux")]
fn namespace_identity(pid: &str) -> std::io::Result<NamespaceIdentity> {
    use std::os::unix::fs::MetadataExt;

    fn identity(path: impl AsRef<std::path::Path>) -> std::io::Result<(u64, u64)> {
        let metadata = std::fs::metadata(path)?;
        Ok((metadata.dev(), metadata.ino()))
    }

    let root = format!("/proc/{pid}/ns");
    Ok(NamespaceIdentity {
        user: identity(format!("{root}/user"))?,
        mount: identity(format!("{root}/mnt"))?,
        pid: identity(format!("{root}/pid"))?,
        ipc: identity(format!("{root}/ipc"))?,
    })
}

#[cfg(target_os = "linux")]
fn peer_pidfd(stream: &wakterm_uds::UnixStream) -> std::io::Result<OwnedFd> {
    use std::mem::size_of;

    let mut pidfd: libc::c_int = -1;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut pidfd as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != size_of::<libc::c_int>() || pidfd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd) })
}

#[cfg(target_os = "linux")]
fn pidfd_is_alive(pidfd: &OwnedFd) -> std::io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result == 0)
    }
}

#[cfg(target_os = "linux")]
fn classify_linux_peer(
    peer_pid: u32,
    peer_uid: u32,
    host_uid: u32,
    host_namespaces: Option<NamespaceIdentity>,
    peer_namespaces: Option<NamespaceIdentity>,
) -> ConnectionAuthority {
    if peer_uid == host_uid && host_namespaces == peer_namespaces && host_namespaces.is_some() {
        ConnectionAuthority::Host
    } else {
        ConnectionAuthority::RestrictedLocal { peer_pid }
    }
}

#[cfg(target_os = "linux")]
fn local_connection_authority(stream: &wakterm_uds::UnixStream) -> ConnectionAuthority {
    use std::mem::{size_of, MaybeUninit};

    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != size_of::<libc::ucred>() {
        log::warn!("Could not establish local mux peer credentials; denying control authority");
        return ConnectionAuthority::RestrictedLocal { peer_pid: 0 };
    }
    let credentials = unsafe { credentials.assume_init() };
    let peer_pid = u32::try_from(credentials.pid).unwrap_or(0);
    let peer_pidfd = match peer_pidfd(stream) {
        Ok(pidfd) => pidfd,
        Err(err) => {
            log::warn!(
                "Could not bind local mux peer PID {peer_pid} to a pidfd: {err}; denying control authority"
            );
            return ConnectionAuthority::RestrictedLocal { peer_pid };
        }
    };
    if !pidfd_is_alive(&peer_pidfd).unwrap_or(false) {
        log::warn!(
            "Local mux peer PID {peer_pid} exited before namespace verification; denying control authority"
        );
        return ConnectionAuthority::RestrictedLocal { peer_pid };
    }
    let peer_namespaces = namespace_identity(&peer_pid.to_string()).ok();
    if !pidfd_is_alive(&peer_pidfd).unwrap_or(false) {
        log::warn!(
            "Local mux peer PID {peer_pid} exited during namespace verification; denying control authority"
        );
        return ConnectionAuthority::RestrictedLocal { peer_pid };
    }
    let authority = classify_linux_peer(
        peer_pid,
        credentials.uid,
        unsafe { libc::geteuid() },
        namespace_identity("self").ok(),
        peer_namespaces,
    );
    if matches!(authority, ConnectionAuthority::RestrictedLocal { .. }) {
        log::warn!(
            "Local mux peer PID {peer_pid} does not share the server's user, mount, PID, and IPC namespaces; granting passive access only"
        );
    }
    authority
}

#[cfg(not(target_os = "linux"))]
fn local_connection_authority(_stream: &wakterm_uds::UnixStream) -> ConnectionAuthority {
    ConnectionAuthority::Host
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn linux_peer_requires_same_uid_and_kernel_namespaces() {
        let host = NamespaceIdentity {
            user: (1, 1),
            mount: (1, 2),
            pid: (1, 3),
            ipc: (1, 4),
        };
        assert_eq!(
            classify_linux_peer(42, 1000, 1000, Some(host), Some(host)),
            ConnectionAuthority::Host
        );

        let sandbox = NamespaceIdentity {
            mount: (2, 2),
            ..host
        };
        assert_eq!(
            classify_linux_peer(43, 1000, 1000, Some(host), Some(sandbox)),
            ConnectionAuthority::RestrictedLocal { peer_pid: 43 }
        );
        assert_eq!(
            classify_linux_peer(44, 1001, 1000, Some(host), Some(host)),
            ConnectionAuthority::RestrictedLocal { peer_pid: 44 }
        );
        assert_eq!(
            classify_linux_peer(45, 1000, 1000, Some(host), None),
            ConnectionAuthority::RestrictedLocal { peer_pid: 45 }
        );
    }

    #[test]
    fn current_process_unix_peer_has_host_authority() {
        let (client, server) = wakterm_uds::UnixStream::pair().unwrap();
        assert_eq!(
            local_connection_authority(&server),
            ConnectionAuthority::Host
        );
        drop(client);
    }
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
fn safely_create_sock_path(unix_dom: &UnixDomain) -> anyhow::Result<UnixListener> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    // We want to remove the socket if it exists.
    // However, on windows, we can't tell if the unix domain socket
    // exists using the methods on Path, so instead we just unconditionally
    // remove it and see what error occurs.
    match std::fs::remove_file(sock_path) {
        Ok(_) => {}
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {}
            _ => return Err(err).context(format!("Unable to remove {}", sock_path.display())),
        },
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(&sock_path);

    Ok(listener)
}
