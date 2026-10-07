use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::Duration};

use serde::{de::Deserializer, Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevdConfig {
    pub version: String,
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub command: String,
    /// TCP addresses this service intends to bind; used by `devd doctor`.
    #[serde(default)]
    pub listen: Vec<SocketAddr>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub requires: Vec<PathRequirement>,
    /// Observe path conditions while running; never authorizes recovery.
    #[serde(default, rename = "monitor-requires", alias = "monitor_requires")]
    pub monitor_requires: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, rename = "env-file", alias = "env_file")]
    pub env_file: Option<PathBuf>,
    #[serde(default, rename = "depends-on", alias = "depends_on")]
    pub depends_on: Vec<Dependency>,
    /// Restart this service after a dependency recovers; disabled by default.
    #[serde(
        default,
        rename = "restart-on-dep-recovery",
        alias = "restart_on_dep_recovery"
    )]
    pub restart_on_dep_recovery: bool,
    #[serde(default)]
    pub healthcheck: Option<HealthCheck>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub limits: Option<ResourceLimits>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub struct PathRequirement {
    #[serde(rename = "type")]
    pub kind: PathRequirementType,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PathRequirementType {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub service: String,
    pub condition: DependencyCondition,
}

impl<'de> Deserialize<'de> for Dependency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged, deny_unknown_fields)]
        enum DependencyInput {
            Name(String),
            Detailed {
                service: String,
                #[serde(default)]
                condition: DependencyCondition,
            },
        }

        match DependencyInput::deserialize(deserializer)? {
            DependencyInput::Name(service) => Ok(Self {
                service,
                condition: DependencyCondition::default(),
            }),
            DependencyInput::Detailed { service, condition } => Ok(Self { service, condition }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyCondition {
    #[default]
    Started,
    SocketReady,
    TcpReady,
    HttpReady,
    ScriptReady,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum HealthCheck {
    #[serde(rename = "script")]
    Script {
        command: String,
        #[serde(
            default = "default_health_interval",
            deserialize_with = "deserialize_duration"
        )]
        interval: Duration,
        #[serde(
            default = "default_health_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
        #[serde(default = "default_health_retries")]
        retries: u32,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(
            default = "default_health_interval",
            deserialize_with = "deserialize_duration"
        )]
        interval: Duration,
        #[serde(
            default = "default_health_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
        #[serde(default = "default_health_retries")]
        retries: u32,
    },
    #[serde(rename = "tcp")]
    Tcp {
        #[serde(default = "default_health_host")]
        host: String,
        port: u16,
        #[serde(
            default = "default_health_interval",
            deserialize_with = "deserialize_duration"
        )]
        interval: Duration,
        #[serde(
            default = "default_health_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
        #[serde(default = "default_health_retries")]
        retries: u32,
    },
    #[serde(rename = "socket")]
    Socket {
        path: PathBuf,
        #[serde(
            default = "default_health_interval",
            deserialize_with = "deserialize_duration"
        )]
        interval: Duration,
        #[serde(
            default = "default_health_timeout",
            deserialize_with = "deserialize_duration"
        )]
        timeout: Duration,
        #[serde(default = "default_health_retries")]
        retries: u32,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RestartPolicy {
    #[serde(default)]
    pub policy: RestartPolicyType,
    #[serde(default)]
    pub backoff: BackoffType,
    #[serde(
        default = "default_initial_delay",
        rename = "initial-delay",
        alias = "initial_delay",
        deserialize_with = "deserialize_duration"
    )]
    pub initial_delay: Duration,
    #[serde(
        default = "default_max_delay",
        rename = "max-delay",
        alias = "max_delay",
        deserialize_with = "deserialize_duration"
    )]
    pub max_delay: Duration,
    #[serde(
        default = "default_max_attempts",
        rename = "max-attempts",
        alias = "max_attempts"
    )]
    pub max_attempts: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            policy: RestartPolicyType::default(),
            backoff: BackoffType::default(),
            initial_delay: default_initial_delay(),
            max_delay: default_max_delay(),
            max_attempts: default_max_attempts(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicyType {
    #[default]
    OnFailure,
    Always,
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackoffType {
    #[default]
    Fixed,
    Exponential,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    /// Automatic intervention requires explicit per-service authorization.
    #[serde(default, rename = "on-exceed", alias = "on_exceed")]
    pub on_exceed: ResourceLimitAction,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceLimitAction {
    #[default]
    Warn,
    Restart,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceThresholds {
    pub cpu_percent: Option<u32>,
    pub memory_bytes: Option<u64>,
    pub on_exceed: ResourceLimitAction,
}

impl ResourceLimits {
    pub(crate) fn thresholds(&self) -> Option<ResourceThresholds> {
        let cpu_percent = match self.cpu.as_deref() {
            Some(value) => Some(parse_cpu_limit(value)?),
            None => None,
        };
        let memory_bytes = match self.memory.as_deref() {
            Some(value) => Some(parse_memory_limit(value)?),
            None => None,
        };
        (cpu_percent.is_some() || memory_bytes.is_some()).then_some(ResourceThresholds {
            cpu_percent,
            memory_bytes,
            on_exceed: self.on_exceed,
        })
    }
}

pub(super) fn parse_cpu_limit(value: &str) -> Option<u32> {
    let digits = value.trim().strip_suffix('%')?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|v| *v > 0)
}

pub(super) fn parse_memory_limit(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value.find(|ch: char| !ch.is_ascii_digit())?;
    let amount: u64 = value[..digits].parse().ok()?;
    let multiplier: u64 = match &value[digits..] {
        "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => return None,
    };
    amount.checked_mul(multiplier).filter(|v| *v > 0)
}

fn default_health_interval() -> Duration {
    Duration::from_secs(10)
}

fn default_health_host() -> String {
    "127.0.0.1".to_owned()
}

fn default_health_timeout() -> Duration {
    Duration::from_secs(2)
}

fn default_health_retries() -> u32 {
    3
}

fn default_initial_delay() -> Duration {
    Duration::from_secs(1)
}

fn default_max_delay() -> Duration {
    Duration::from_secs(60)
}

fn default_max_attempts() -> u32 {
    3
}

fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;

    match value {
        serde_yaml::Value::Number(number) => number
            .as_u64()
            .map(Duration::from_secs)
            .ok_or_else(|| serde::de::Error::custom("duration must be a non-negative integer")),
        serde_yaml::Value::String(value) => parse_duration(&value)
            .map_err(|error| serde::de::Error::custom(format!("invalid duration: {error}"))),
        _ => Err(serde::de::Error::custom(
            "duration must be an integer number of seconds or a string such as '5s'",
        )),
    }
}

pub(crate) fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, unit) = value.split_at(
        value
            .find(|character: char| !character.is_ascii_digit())
            .ok_or_else(|| "missing duration unit".to_owned())?,
    );
    let number: u64 = number
        .parse()
        .map_err(|_| "invalid duration number".to_owned())?;

    match unit {
        "ms" => Ok(Duration::from_millis(number)),
        "s" => Ok(Duration::from_secs(number)),
        "m" => Duration::from_secs(number)
            .checked_mul(60)
            .ok_or_else(|| "duration is too large".to_owned()),
        "h" => Duration::from_secs(number)
            .checked_mul(60 * 60)
            .ok_or_else(|| "duration is too large".to_owned()),
        _ => Err(format!("unsupported duration unit '{unit}'")),
    }
}
