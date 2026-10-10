mod doctor;
mod events;
mod explain;
mod graph;
mod instances;
mod protocol;
mod reload;
mod server;
mod snapshot;
#[cfg(unix)]
mod stdout;
#[cfg(windows)]
#[path = "stdout_windows.rs"]
mod stdout;
mod top;
mod transport;

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::{
    config::{parse_duration, validate_profile_name, ConfigLoader, DevdConfig},
    core::service_manager::{ManagerOptions, ServiceManager},
    logging::{ColorMode, LogFilter, LogFormatter, LogLevel},
    storage::StorageOptions,
};
use protocol::{Request, Response};

#[derive(Parser)]
#[command(name = "devd", version, about = "Manage local development services")]
pub struct Cli {
    /// Configuration file; service cwd paths are relative to its directory.
    #[arg(short, long, global = true, default_value = "devd.yml")]
    config: PathBuf,
    /// Select a named configuration overlay and its isolated runtime instance.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Override the project runtime directory (use a short path for Unix sockets).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "auto")]
    color: Color,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Color {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
enum Command {
    /// Start all services in the foreground; Ctrl+C stops the stack.
    Start {
        /// Persist JSONL logs in the instance's state directory.
        #[arg(long)]
        persist_logs: bool,
        /// Maximum size of new log files in MiB (1–1024).
        #[arg(long, requires = "persist_logs", value_parser = clap::value_parser!(u16).range(1..=1024))]
        log_max_size: Option<u16>,
        /// Number of rotated log files to retain, in addition to the current file.
        #[arg(long, requires = "persist_logs", value_parser = clap::value_parser!(u16).range(1..=100))]
        log_keep: Option<u16>,
        /// Persist lifecycle events separately from application logs.
        #[arg(long)]
        persist_events: bool,
        /// Maximum event file size in MiB (1–1024).
        #[arg(long, requires = "persist_events", value_parser = clap::value_parser!(u16).range(1..=1024))]
        event_max_size: Option<u16>,
        /// Number of event archives retained in addition to the current file.
        #[arg(long, requires = "persist_events", value_parser = clap::value_parser!(u16).range(1..=100))]
        event_keep: Option<u16>,
    },
    /// Request ordered shutdown of the running supervisor.
    Stop,
    /// Stop and start one service using the running configuration.
    Restart { service: String },
    /// Preview or explicitly apply configuration changes to a running supervisor.
    Reload(reload::Args),
    /// Query live service states and PIDs.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Inspect and control a running stack in an interactive terminal.
    Top,
    /// List registered instances in this project and its Git worktrees (read-only).
    Instances {
        #[arg(long)]
        json: bool,
    },
    /// Query the identity of the selected live supervisor.
    Identity {
        #[arg(long)]
        json: bool,
    },
    /// Query lifecycle history and explicit gaps, or follow a running stack.
    Events(events::Args),
    /// Explain the latest deterministic cause for one service.
    Explain(explain::Args),
    /// Inspect local service prerequisites without starting or executing them.
    Doctor(doctor::Args),
    /// Print buffered logs, or query stored logs after shutdown (oldest first).
    Logs {
        service: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        tail: u16,
        /// Match one exact log level.
        #[arg(long, value_enum)]
        level: Option<LogLevel>,
        /// Include entries from the last duration (for example, 5m or 2h).
        #[arg(long, value_parser = parse_duration)]
        since: Option<std::time::Duration>,
        /// Match this literal, case-sensitive text in the raw message.
        #[arg(long)]
        grep: Option<String>,
        /// Continue printing new entries until Ctrl+C or the supervisor stops.
        #[arg(short, long)]
        follow: bool,
        /// Read disk logs after the persistent supervisor stops; no YAML needed.
        #[arg(long, conflicts_with = "follow")]
        stored: bool,
    },
    /// Validate configuration and supported MVP settings without starting services.
    Check,
    /// Display dependencies and parallel startup layers, or export a diagram.
    Graph {
        /// Output format. Diagram arrows run from prerequisites to dependents.
        #[arg(long, value_enum, default_value_t = graph::GraphFormat::Text)]
        format: graph::GraphFormat,
    },
    /// Create a starter configuration without replacing an existing file.
    Init {
        /// Name of the first service.
        #[arg(long, default_value = "app")]
        service: String,
        /// Command to start the first service.
        #[arg(long)]
        #[cfg_attr(unix, arg(default_value = "sh -c 'echo app-ready; exec sleep 3600'"))]
        #[cfg_attr(
            windows,
            arg(
                default_value = "powershell.exe -NoLogo -NoProfile -Command 'Write-Output app-ready; Start-Sleep -Seconds 3600'"
            )
        )]
        command: String,
    },
    /// Save a copy of the YAML configuration or restore it to a new file.
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Save the complete on-disk YAML, including all profiles.
    Save { name: String },
    /// Restore a saved YAML to a new file beside the original configuration.
    Restore {
        name: String,
        /// New filename in the original configuration directory; never overwritten.
        #[arg(long)]
        output: PathBuf,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        if let Command::Instances { json } = self.command {
            if self.profile.is_some()
                || self.state_dir.is_some()
                || self.config != Path::new("devd.yml")
            {
                bail!("instances discovers the current project; --config, --profile and --state-dir are not supported");
            }
            return instances::list(&std::env::current_dir()?, json).await;
        }
        if let Some(profile) = &self.profile {
            validate_profile_name(profile)?;
            match self.command {
                Command::Init { .. } => {
                    bail!("--profile is not supported by init; add profiles to the generated YAML");
                }
                Command::Snapshot { .. } => {
                    bail!("--profile is not supported by snapshot; snapshots contain the whole YAML, including all profiles");
                }
                _ => {}
            }
        }
        let config_path = absolute_config(&self.config).await?;
        let mut state_dir = match self.state_dir {
            Some(path) => path,
            None => config_path
                .parent()
                .unwrap()
                .join(".devd")
                .join(config_path.file_name().unwrap()),
        };
        if let Some(profile) = &self.profile {
            state_dir = state_dir.join("profiles").join(profile_directory(profile));
        }
        let socket = state_dir.join("control.sock");
        let mut options = ManagerOptions::new(state_dir.join("services.json"));
        options.profile = self.profile.clone();
        let formatter = LogFormatter {
            color: match self.color {
                Color::Auto => ColorMode::Auto,
                Color::Always => ColorMode::Always,
                Color::Never => ColorMode::Never,
            },
        };
        match self.command {
            Command::Init { service, command } => {
                let yaml = serde_yaml::to_string(&serde_json::json!({
                    "version": "1",
                    "services": { (service): { "command": command } },
                }))?;
                let config = ConfigLoader::from_str(&yaml, &config_path)?;
                ServiceManager::new(config, options)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&config_path)
                    .await
                    .with_context(|| {
                        format!(
                            "cannot create {}; an existing configuration will not be replaced",
                            config_path.display()
                        )
                    })?;
                file.write_all(yaml.as_bytes()).await?;
                file.flush().await?;
                output(&format!("Created {}\n", config_path.display()))?;
            }
            Command::Snapshot { action } => match action {
                SnapshotAction::Save { name } => {
                    let path = snapshot::save(&config_path, &state_dir, &name).await?;
                    output(&format!("Saved snapshot {}\n", path.display()))?;
                }
                SnapshotAction::Restore { name, output: file } => {
                    let path = snapshot::restore(&config_path, &state_dir, &name, &file).await?;
                    output(&format!("Restored configuration to {}\n", path.display()))?;
                }
            },
            Command::Start {
                persist_logs,
                log_max_size,
                log_keep,
                persist_events,
                event_max_size,
                event_keep,
            } => {
                let config = load_config(&config_path, self.profile.as_deref()).await?;
                let manager = ServiceManager::new(config, options.clone())?;
                let storage = persist_logs.then(|| StorageOptions {
                    max_file_bytes: u64::from(log_max_size.unwrap_or(10)) * 1024 * 1024,
                    keep: log_keep.unwrap_or(3),
                });
                let event_storage = persist_events.then(|| StorageOptions {
                    max_file_bytes: u64::from(event_max_size.unwrap_or(10)) * 1024 * 1024,
                    keep: event_keep.unwrap_or(3),
                });
                server::start(
                    manager,
                    options,
                    socket,
                    formatter,
                    storage,
                    event_storage,
                    config_path,
                )
                .await?;
            }
            Command::Events(args) => events::run(args, &socket, &state_dir).await?,
            Command::Identity { json } => instances::show(&socket, json).await?,
            Command::Instances { .. } => unreachable!("handled before configuration resolution"),
            Command::Explain(args) => explain::run(args, &socket, &state_dir).await?,
            Command::Reload(args) => reload::run(args, &socket, &config_path).await?,
            Command::Doctor(args) => {
                doctor::run(args, &config_path, self.profile.as_deref()).await?;
            }
            Command::Check => {
                ServiceManager::new(
                    load_config(&config_path, self.profile.as_deref()).await?,
                    options,
                )?;
                let profile = self
                    .profile
                    .as_ref()
                    .map_or_else(String::new, |name| format!(" (profile: {name})"));
                output(&format!(
                    "Configuration valid: {}{profile}\n",
                    config_path.display()
                ))?;
            }
            Command::Graph { format } => {
                let config = load_config(&config_path, self.profile.as_deref()).await?;
                ServiceManager::new(config.clone(), options)?;
                output(&graph::render(&config, format)?)?;
            }
            Command::Status { json } => {
                let Response::Status(snapshot) =
                    protocol::request(&socket, Request::Status).await?
                else {
                    bail!("unexpected status response");
                };
                if json {
                    output(&format!("{}\n", serde_json::to_string_pretty(&snapshot)?))?;
                } else {
                    let mut text = format!(
                        "Supervisor PID: {}\nSERVICE\tSTATE\tPID\tRESTARTS\tCPU %\tRSS MiB\n",
                        snapshot.supervisor_pid
                    );
                    for (name, state) in snapshot.services {
                        text.push_str(&format!(
                            "{name}\t{:?}\t{}\t{}\t{}\t{}\n",
                            state.status,
                            state.pid.map_or_else(|| "-".into(), |pid| pid.to_string()),
                            state.restart_count,
                            state
                                .resources
                                .as_ref()
                                .and_then(|r| r.cpu_percent)
                                .map_or_else(|| "-".into(), |cpu| format!("{cpu:.1}")),
                            state.resources.as_ref().map_or_else(
                                || "-".into(),
                                |r| format!("{:.1}", r.memory_bytes as f64 / 1_048_576.0)
                            )
                        ));
                        if let Some(error) = state.last_error {
                            text.push_str(&format!("  {error:?}\n"));
                        }
                    }
                    output(&text)?;
                }
            }
            Command::Top => {
                let color = match self.color {
                    Color::Auto => colored::control::SHOULD_COLORIZE.should_colorize(),
                    Color::Always => true,
                    Color::Never => false,
                };
                top::run(&socket, color).await?;
            }
            Command::Stop => {
                let Response::Stopping = protocol::request(&socket, Request::Stop).await? else {
                    bail!("unexpected stop response");
                };
                output("Shutdown requested; the foreground supervisor exits after cleanup.\n")?;
            }
            Command::Restart { service } => {
                let Response::Restarted(state) = protocol::request(
                    &socket,
                    Request::Restart {
                        service: service.clone(),
                    },
                )
                .await?
                else {
                    bail!("unexpected restart response");
                };
                output(&format!(
                    "Restarted {service} (PID {})\n",
                    state.pid.context("restart returned no PID")?
                ))?;
            }
            Command::Logs {
                service,
                tail,
                level,
                since,
                grep,
                stored: true,
                ..
            } => {
                let filter = log_filter(level, since, grep)?;
                let directory = state_dir.join("logs");
                let entries = tokio::task::spawn_blocking(move || {
                    crate::logging::storage::read_stored(
                        &directory,
                        service.as_deref(),
                        &filter,
                        tail.into(),
                    )
                })
                .await
                .context("stored log reader task failed")?
                .context("cannot read stored logs; enable persistence at startup and query after shutdown")?;
                let text: String = entries
                    .iter()
                    .map(|entry| formatter.format(entry))
                    .collect();
                output(&text)?;
            }
            Command::Logs {
                service,
                tail,
                level,
                since,
                grep,
                follow,
                ..
            } if follow => {
                let filter = log_filter(level, since, grep)?;
                let mut stream = protocol::connect(&socket).await?;
                protocol::write(
                    &mut stream,
                    &Request::FollowLogs {
                        service,
                        tail: tail.into(),
                        filter,
                    },
                )
                .await?;
                let first = tokio::time::timeout(
                    protocol::IO_TIMEOUT,
                    protocol::next_response(&mut stream),
                )
                .await
                .context("log stream did not start")??
                .context("supervisor closed the log stream before responding")?;
                let Response::Logs(entries) = first else {
                    match first {
                        Response::Error(error) => bail!("{error}"),
                        _ => bail!("unexpected logs response"),
                    }
                };
                let mut stdout = stdout::Stdout::new()?;
                for entry in entries {
                    let line = formatter.format(&entry);
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => return Ok(()),
                        result = async {
                            stdout.write_all(line.as_bytes()).await?;
                            stdout.flush().await
                        } => result?,
                    }
                }
                loop {
                    let response = tokio::select! {
                        _ = tokio::signal::ctrl_c() => break,
                        response = protocol::next_response(&mut stream) => response?,
                    };
                    match response {
                        Some(Response::Log(entry)) => {
                            let line = formatter.format(&entry);
                            tokio::select! {
                                _ = tokio::signal::ctrl_c() => break,
                                result = async {
                                    stdout.write_all(line.as_bytes()).await?;
                                    stdout.flush().await
                                } => result?,
                            }
                        }
                        Some(Response::Error(error)) => bail!("{error}"),
                        None => break,
                        _ => bail!("unexpected log stream response"),
                    }
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    result = stdout.flush() => result?,
                }
            }
            Command::Logs {
                service,
                tail,
                level,
                since,
                grep,
                ..
            } => {
                let filter = log_filter(level, since, grep)?;
                let Response::Logs(entries) = protocol::request(
                    &socket,
                    Request::Logs {
                        service,
                        tail: tail.into(),
                        filter,
                    },
                )
                .await?
                else {
                    bail!("unexpected logs response");
                };
                let text: String = entries
                    .iter()
                    .map(|entry| formatter.format(entry))
                    .collect();
                output(&text)?;
            }
        }
        Ok(())
    }
}

fn log_filter(
    level: Option<LogLevel>,
    since: Option<std::time::Duration>,
    grep: Option<String>,
) -> Result<LogFilter> {
    let since = since
        .map(|duration| {
            let duration =
                chrono::Duration::from_std(duration).context("--since duration is too large")?;
            Utc::now()
                .checked_sub_signed(duration)
                .context("--since duration is too large")
        })
        .transpose()?;
    Ok(LogFilter { level, since, grep })
}

async fn absolute_config(path: &Path) -> Result<PathBuf> {
    // Existing symlinks share an endpoint. A removed/broken config can still be
    // addressed by its original filename so stop never needs to parse YAML.
    if let Ok(path) = tokio::fs::canonicalize(path).await {
        return Ok(path);
    }
    let name = path
        .file_name()
        .context("--config must name a configuration file")?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(tokio::fs::canonicalize(parent)
        .await
        .with_context(|| format!("cannot locate config directory {}", parent.display()))?
        .join(name))
}

fn profile_directory(name: &str) -> String {
    #[cfg(windows)]
    {
        // Windows reserves device names (CON, AUX, ...) even with extensions,
        // and strips trailing dots. Encode every byte to preserve all accepted
        // profile identities without changing the portable config schema.
        name.bytes().fold(String::from("p-"), |mut path, byte| {
            path.push_str(&format!("{byte:02x}"));
            path
        })
    }
    #[cfg(unix)]
    {
        // Escape uppercase bytes to keep case-sensitive profile identities distinct
        // on case-insensitive filesystems. '~' cannot appear in a profile name.
        name.bytes().fold(String::new(), |mut path, byte| {
            if byte.is_ascii_uppercase() {
                path.push_str(&format!("~{byte:02x}"));
            } else {
                path.push(char::from(byte));
            }
            path
        })
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn test_profile_directories_preserve_windows_device_case_and_dot_names() {
        let names = ["con", "CON", "con.", "aux", "aux.txt", "prod", "prod."];
        let root = tempfile::tempdir().unwrap();
        for name in names {
            std::fs::create_dir(root.path().join(profile_directory(name))).unwrap();
        }
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), names.len());
    }
}

async fn load_config(path: &Path, profile: Option<&str>) -> Result<DevdConfig> {
    let mut config = ConfigLoader::new().load_profile(path, profile).await?;
    resolve_working_directories(&mut config, path);
    Ok(config)
}

fn resolve_working_directories(config: &mut DevdConfig, path: &Path) {
    for service in config.services.values_mut() {
        service.cwd = Some(
            path.parent()
                .unwrap()
                .join(service.cwd.as_deref().unwrap_or(Path::new("."))),
        );
    }
}

fn output(text: &str) -> Result<()> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .context("cannot write output")
}
