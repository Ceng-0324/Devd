mod protocol;
mod server;
mod stdout;

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::{
    config::{validate_profile_name, ConfigLoader, DevdConfig},
    core::{
        dependency::DependencyGraph,
        service_manager::{ManagerOptions, ServiceManager},
    },
    logging::{ColorMode, LogFormatter},
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
    Start,
    /// Request ordered shutdown of the running supervisor.
    Stop,
    /// Stop and start one service using the running configuration.
    Restart { service: String },
    /// Query live service states and PIDs.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print buffered logs (memory only, oldest first).
    Logs {
        service: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=1000))]
        tail: u16,
        /// Continue printing new entries until Ctrl+C or the supervisor stops.
        #[arg(short, long)]
        follow: bool,
    },
    /// Validate configuration and supported MVP settings without starting services.
    Check,
    /// Display dependencies and parallel startup layers.
    Graph,
    /// Create a starter configuration without replacing an existing file.
    Init {
        /// Name of the first service.
        #[arg(long, default_value = "app")]
        service: String,
        /// Command to start the first service.
        #[arg(long, default_value = "sh -c 'echo app-ready; exec sleep 3600'")]
        command: String,
    },
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        if let Some(profile) = &self.profile {
            validate_profile_name(profile)?;
            if matches!(self.command, Command::Init { .. }) {
                bail!("--profile is not supported by init; add profiles to the generated YAML");
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
        let options = ManagerOptions::new(state_dir.join("services.json"));
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
            Command::Start => {
                let config = load_config(&config_path, self.profile.as_deref()).await?;
                let manager = ServiceManager::new(config, options.clone())?;
                server::start(manager, options, socket, formatter).await?;
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
            Command::Graph => {
                let config = load_config(&config_path, self.profile.as_deref()).await?;
                ServiceManager::new(config.clone(), options)?;
                let graph = DependencyGraph::from_config(&config)?;
                let mut text = String::from("Dependencies (service -> prerequisite):\n");
                for name in graph.service_names() {
                    let service = &config.services[name];
                    if service.depends_on.is_empty() {
                        text.push_str(&format!("  {name}\n"));
                    }
                    let mut edges: Vec<_> = service.depends_on.iter().collect();
                    edges.sort_by(|a, b| a.service.cmp(&b.service));
                    for edge in edges {
                        text.push_str(&format!(
                            "  {name} -> {} ({:?})\n",
                            edge.service, edge.condition
                        ));
                    }
                }
                text.push_str("Startup layers:\n");
                for (index, layer) in graph.startup_layers()?.iter().enumerate() {
                    text.push_str(&format!("  {}: {}\n", index + 1, layer.join(", ")));
                }
                output(&text)?;
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
                follow,
            } if follow => {
                let mut stream = protocol::connect(&socket).await?;
                protocol::write(
                    &mut stream,
                    &Request::FollowLogs {
                        service,
                        tail: tail.into(),
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
                        result = stdout.write_all(line.as_bytes()) => result?,
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
                                result = stdout.write_all(line.as_bytes()) => result?,
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
            Command::Logs { service, tail, .. } => {
                let Response::Logs(entries) = protocol::request(
                    &socket,
                    Request::Logs {
                        service,
                        tail: tail.into(),
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

async fn load_config(path: &Path, profile: Option<&str>) -> Result<DevdConfig> {
    let mut config = ConfigLoader::new().load_profile(path, profile).await?;
    for service in config.services.values_mut() {
        service.cwd = Some(
            path.parent()
                .unwrap()
                .join(service.cwd.as_deref().unwrap_or(Path::new("."))),
        );
    }
    Ok(config)
}

fn output(text: &str) -> Result<()> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .context("cannot write output")
}
