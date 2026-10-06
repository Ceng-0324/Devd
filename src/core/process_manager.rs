use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use nix::{
    errno::Errno,
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use thiserror::Error;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::config::ServiceConfig;

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("service '{service}' has an empty command")]
    EmptyCommand { service: String },
    #[error("failed to parse command for service '{service}': {source}")]
    CommandParse {
        service: String,
        #[source]
        source: shell_words::ParseError,
    },
    #[error("failed to read environment file {path} for service '{service}': {source}")]
    EnvironmentRead {
        service: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse environment file {path} for service '{service}': {source}")]
    EnvironmentParse {
        service: String,
        path: PathBuf,
        #[source]
        source: dotenvy::Error,
    },
    #[error("failed to spawn service '{service}' with command {command:?} in {cwd:?}: {source}")]
    Spawn {
        service: String,
        command: String,
        cwd: Option<PathBuf>,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to wait for service '{service}': {source}")]
    Wait {
        service: String,
        #[source]
        source: std::io::Error,
    },
    #[error("process group {group} for service '{service}' did not disappear after SIGKILL")]
    CleanupTimeout { service: String, group: i32 },
    #[error("stop grace period {grace_period:?} is too large for service '{service}'")]
    InvalidGracePeriod {
        service: String,
        grace_period: Duration,
    },
    #[error("failed to send {signal:?} to process group for service '{service}': {source}")]
    Signal {
        service: String,
        signal: Signal,
        #[source]
        source: Errno,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running { pid: u32 },
    Exited { status: ExitStatus },
}

/// Owns one Unix service process and its process group. Operations do not schedule
/// dependencies or drain logs. Callers must drain stdout and stderr concurrently
/// to avoid a child blocking on full pipes.
#[derive(Debug)]
pub struct ManagedProcess {
    service: String,
    config: ServiceConfig,
    child: Child,
    group: Option<Pid>,
    capture_output: bool,
}

impl ManagedProcess {
    /// Spawn a command using shell-style argument quoting, without an implicit
    /// shell. `cwd` is relative to devd's working directory; `env-file` is relative
    /// to the service's `cwd`. Explicit `env` values override the environment file.
    pub async fn spawn(
        service: impl Into<String>,
        config: &ServiceConfig,
    ) -> Result<Self, ProcessError> {
        Self::spawn_with_output(service.into(), config, true).await
    }

    /// Health probes discard output, preventing pipe backpressure and leakage
    /// into service logs. Process-group ownership is identical to services.
    pub(super) async fn spawn_probe(config: &ServiceConfig) -> Result<Self, ProcessError> {
        Self::spawn_with_output("healthcheck".into(), config, false).await
    }

    async fn spawn_with_output(
        service: String,
        config: &ServiceConfig,
        capture_output: bool,
    ) -> Result<Self, ProcessError> {
        let (child, group) = spawn_child(&service, config, capture_output).await?;
        Ok(Self {
            service,
            config: config.clone(),
            child,
            group: Some(group),
            capture_output,
        })
    }

    /// PID until the child's exit status has been collected.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Move stdout to the log collector once per process generation.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Move stderr to the log collector once per process generation.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Poll the leader without waiting for its exit. Once it has exited, await
    /// cleanup of its process group before reporting the completed generation.
    pub async fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError> {
        let status = self.child.try_wait().map_err(|source| ProcessError::Wait {
            service: self.service.clone(),
            source,
        })?;
        if status.is_some() {
            self.cleanup_group_async().await?;
        }
        Ok(status)
    }

    pub async fn state(&mut self) -> Result<ProcessState, ProcessError> {
        match self.try_wait().await? {
            Some(status) => Ok(ProcessState::Exited { status }),
            None => Ok(ProcessState::Running {
                pid: self.child.id().expect("unreaped child has a PID"),
            }),
        }
    }

    /// Wait for the leader and clean up remaining group members. Safe to cancel
    /// and retry; the process remains owned by this handle while the future waits.
    pub async fn wait(&mut self) -> Result<ExitStatus, ProcessError> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|source| ProcessError::Wait {
                service: self.service.clone(),
                source,
            })?;
        self.cleanup_group_async().await?;
        Ok(status)
    }

    /// Send SIGTERM to the process group, wait up to `grace_period` for the leader,
    /// then use SIGKILL if necessary. Any descendants left after the leader exits
    /// are killed as well. Repeated stops return the same collected exit status.
    pub async fn stop(&mut self, grace_period: Duration) -> Result<ExitStatus, ProcessError> {
        if let Some(status) = self.try_wait().await? {
            return Ok(status);
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(grace_period)
            .ok_or_else(|| ProcessError::InvalidGracePeriod {
                service: self.service.clone(),
                grace_period,
            })?;
        self.signal_group(Signal::SIGTERM)?;
        match tokio::time::timeout_at(deadline, self.wait()).await {
            Ok(result) => result,
            Err(_) => {
                self.signal_group(Signal::SIGKILL)?;
                self.wait().await
            }
        }
    }

    /// Stop and spawn a new generation using the original configuration snapshot.
    /// Each generation has new stdout/stderr pipes. A spawn failure leaves the
    /// previous generation stopped, with its exit status still available.
    pub async fn restart(&mut self, grace_period: Duration) -> Result<u32, ProcessError> {
        self.stop(grace_period).await?;
        let (child, group) = spawn_child(&self.service, &self.config, self.capture_output).await?;
        self.child = child;
        self.group = Some(group);
        Ok(self.child.id().expect("newly spawned child has a PID"))
    }

    fn signal_group(&self, signal: Signal) -> Result<(), ProcessError> {
        if let Some(group) = self.group {
            match killpg(group, signal) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(source) => {
                    return Err(ProcessError::Signal {
                        service: self.service.clone(),
                        signal,
                        source,
                    })
                }
            }
        }
        Ok(())
    }

    async fn cleanup_group_async(&mut self) -> Result<(), ProcessError> {
        let Some(group) = self.group else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            // A successful group signal is not a completion barrier: a fork in
            // flight can leave a descendant behind. Keep ownership and retry
            // until the kernel reports that the group no longer exists.
            match killpg(group, Signal::SIGKILL) {
                Err(Errno::ESRCH) => {
                    self.group = None;
                    return Ok(());
                }
                Ok(()) => {}
                // Darwin may reject signals to groups containing only exiting
                // zombies. Retry briefly; never silently swallow persistent EPERM.
                Err(Errno::EPERM) if cfg!(target_os = "macos") => {}
                Err(source) => {
                    return Err(ProcessError::Signal {
                        service: self.service.clone(),
                        signal: Signal::SIGKILL,
                        source,
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProcessError::CleanupTimeout {
                    service: self.service.clone(),
                    group: group.as_raw(),
                });
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        // Tokio's kill_on_drop only covers the leader. Kill its process group too.
        let _ = self.signal_group(Signal::SIGKILL);
    }
}

async fn spawn_child(
    service: &str,
    config: &ServiceConfig,
    capture_output: bool,
) -> Result<(Child, Pid), ProcessError> {
    let arguments = parse_command(service, &config.command)?;
    let mut command = Command::new(&arguments[0]);
    command
        .args(&arguments[1..])
        .stdin(Stdio::null())
        .stdout(if capture_output {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(if capture_output {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true)
        .process_group(0);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    if let Some(env_file) = &config.env_file {
        let path = config
            .cwd
            .as_ref()
            .map_or_else(|| env_file.clone(), |cwd| cwd.join(env_file));
        let contents =
            tokio::fs::read(&path)
                .await
                .map_err(|source| ProcessError::EnvironmentRead {
                    service: service.to_owned(),
                    path: path.clone(),
                    source,
                })?;
        for entry in dotenvy::from_read_iter(contents.as_slice()) {
            let (key, value) = entry.map_err(|source| ProcessError::EnvironmentParse {
                service: service.to_owned(),
                path: path.clone(),
                source,
            })?;
            command.env(key, value);
        }
    }
    command.envs(&config.env);
    let child = command.spawn().map_err(|source| ProcessError::Spawn {
        service: service.to_owned(),
        command: config.command.clone(),
        cwd: config.cwd.clone(),
        source,
    })?;
    let group = Pid::from_raw(child.id().expect("newly spawned child has a PID") as i32);
    Ok((child, group))
}

pub(super) fn parse_command(service: &str, command: &str) -> Result<Vec<String>, ProcessError> {
    let arguments = shell_words::split(command).map_err(|source| ProcessError::CommandParse {
        service: service.to_owned(),
        source,
    })?;
    if arguments.first().is_none_or(|program| program.is_empty()) {
        return Err(ProcessError::EmptyCommand {
            service: service.to_owned(),
        });
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_process_spawn_rejects_empty_and_malformed_commands_before_file_io() {
        let mut config: ServiceConfig = serde_yaml::from_str("command: test").unwrap();
        config.env_file = Some("missing-environment-file".into());
        for command in ["", " \t ", "''"] {
            config.command = command.into();
            assert!(matches!(ManagedProcess::spawn("empty", &config).await,
                Err(ProcessError::EmptyCommand { service }) if service == "empty"));
        }
        config.command = "unterminated '".into();
        assert!(matches!(ManagedProcess::spawn("quoting", &config).await,
            Err(ProcessError::CommandParse { service, .. }) if service == "quoting"));
    }
}
