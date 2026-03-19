use crate::config::validate_domain_name;
use crate::*;
use luahelper::impl_lua_conversion_dynamic;
use std::fmt::Display;
use std::str::FromStr;
use wakterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};

#[derive(Debug, Clone, Copy, FromDynamic, ToDynamic)]
pub enum SshBackend {
    Ssh2,
    LibSsh,
}

impl Default for SshBackend {
    fn default() -> Self {
        Self::LibSsh
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshMultiplexing {
    Wakterm,
    None,
    // TODO: Tmux-cc in the future?
}

impl Default for SshMultiplexing {
    fn default() -> Self {
        Self::Wakterm
    }
}

impl SshMultiplexing {
    fn variants() -> &'static [&'static str] {
        &["wakterm", "none"]
    }
}

impl FromDynamic for SshMultiplexing {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, wakterm_dynamic::Error> {
        match value {
            Value::String(s) => match s.as_str() {
                "wakterm" | "WakTerm" | "wezterm" | "WezTerm" => Ok(Self::Wakterm),
                "none" | "None" => Ok(Self::None),
                _ => Err(wakterm_dynamic::Error::InvalidVariantForType {
                    variant_name: s.clone(),
                    type_name: "SshMultiplexing",
                    possible: Self::variants(),
                }),
            },
            other => Err(wakterm_dynamic::Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "SshMultiplexing",
            }),
        }
    }
}

impl ToDynamic for SshMultiplexing {
    fn to_dynamic(&self) -> Value {
        let value = match self {
            Self::Wakterm => "wakterm",
            Self::None => "none",
        };
        Value::String(value.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum Shell {
    /// Unknown command shell: no assumptions can be made
    Unknown,

    /// Posix shell compliant, such that `cd DIR ; exec CMD` behaves
    /// as it does in the bourne shell family of shells
    Posix,
    // TODO: Cmd, PowerShell in the future?
}

impl Default for Shell {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Default, Debug, Clone, FromDynamic, ToDynamic)]
pub struct SshDomain {
    /// The name of this specific domain.  Must be unique amongst
    /// all types of domain in the configuration file.
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,

    /// identifies the host:port pair of the remote server.
    pub remote_address: String,

    /// Whether agent auth should be disabled
    #[dynamic(default)]
    pub no_agent_auth: bool,

    /// The username to use for authenticating with the remote host
    pub username: Option<String>,

    /// If true, connect to this domain automatically at startup
    #[dynamic(default)]
    pub connect_automatically: bool,

    #[dynamic(default = "default_read_timeout")]
    pub timeout: Duration,

    #[dynamic(default = "default_local_echo_threshold_ms")]
    pub local_echo_threshold_ms: Option<u64>,

    /// Show time since last response when waiting for a response.
    /// It is recommended to use
    /// <https://wakterm.org/config/lua/pane/get_metadata.html#since_last_response_ms>
    /// instead.
    #[dynamic(default)]
    pub overlay_lag_indicator: bool,

    /// The path to the wakterm binary on the remote host
    pub remote_wakterm_path: Option<String>,
    /// Override the entire `wakterm cli proxy` invocation that would otherwise
    /// be computed from remote_wakterm_path and other information.
    pub override_proxy_command: Option<String>,

    pub ssh_backend: Option<SshBackend>,

    /// If false, then don't use a multiplexer connection,
    /// just connect directly using ssh. This doesn't require
    /// that the remote host have wakterm installed, and is equivalent
    /// to using `wakterm ssh` to connect.
    #[dynamic(default)]
    pub multiplexing: SshMultiplexing,

    /// ssh_config option values
    #[dynamic(default)]
    pub ssh_option: HashMap<String, String>,

    pub default_prog: Option<Vec<String>>,

    #[dynamic(default)]
    pub assume_shell: Shell,
}
impl_lua_conversion_dynamic!(SshDomain);

impl SshDomain {
    pub fn default_domains() -> Vec<Self> {
        let mut config = wakterm_ssh::Config::new();
        config.add_default_config_files();

        let mut plain_ssh = vec![];
        let mut mux_ssh = vec![];
        for host in config.enumerate_hosts() {
            plain_ssh.push(Self {
                name: format!("SSH:{host}"),
                remote_address: host.to_string(),
                multiplexing: SshMultiplexing::None,
                local_echo_threshold_ms: default_local_echo_threshold_ms(),
                ..SshDomain::default()
            });

            mux_ssh.push(Self {
                name: format!("SSHMUX:{host}"),
                remote_address: host.to_string(),
                multiplexing: SshMultiplexing::Wakterm,
                local_echo_threshold_ms: default_local_echo_threshold_ms(),
                ..SshDomain::default()
            });
        }

        plain_ssh.append(&mut mux_ssh);
        plain_ssh
    }
}

#[cfg(test)]
mod test {
    use super::SshMultiplexing;
    use wakterm_dynamic::{FromDynamic, FromDynamicOptions, ToDynamic, Value};

    #[test]
    fn ssh_multiplexing_serializes_to_lowercase_wakterm() {
        assert_eq!(
            SshMultiplexing::Wakterm.to_dynamic(),
            Value::String("wakterm".to_string())
        );
    }

    #[test]
    fn ssh_multiplexing_accepts_legacy_and_renamed_values() {
        for value in ["wakterm", "WakTerm", "wezterm", "WezTerm"] {
            assert_eq!(
                SshMultiplexing::from_dynamic(
                    &Value::String(value.to_string()),
                    FromDynamicOptions::default()
                )
                .unwrap(),
                SshMultiplexing::Wakterm
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct SshParameters {
    pub username: Option<String>,
    pub host_and_port: String,
}

impl Display for SshParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(user) = &self.username {
            write!(f, "{}@{}", user, self.host_and_port)
        } else {
            write!(f, "{}", self.host_and_port)
        }
    }
}

pub fn username_from_env() -> anyhow::Result<String> {
    #[cfg(unix)]
    const USER: &str = "USER";
    #[cfg(windows)]
    const USER: &str = "USERNAME";

    std::env::var(USER).with_context(|| format!("while resolving {} env var", USER))
}

impl FromStr for SshParameters {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('@').collect();

        if parts.len() == 2 {
            Ok(Self {
                username: Some(parts[0].to_string()),
                host_and_port: parts[1].to_string(),
            })
        } else if parts.len() == 1 {
            Ok(Self {
                username: None,
                host_and_port: parts[0].to_string(),
            })
        } else {
            bail!("failed to parse ssh parameters from `{}`", s);
        }
    }
}
