use std::{future::Future, io, time::Duration};

use reqwest::{Client, StatusCode, Url};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    time::{Instant, Interval, MissedTickBehavior},
};

use crate::config::{ConfigValidationError, HealthCheck, ServiceConfig};

#[derive(Debug, Error)]
pub enum HealthCheckError {
    #[error("Unix socket health checks are not supported on Windows; use TCP, HTTP, or script")]
    UnsupportedSocket,
    #[error(transparent)]
    InvalidConfig(#[from] ConfigValidationError),
    #[error("failed to construct HTTP health-check client: {source}")]
    Client {
        #[source]
        source: reqwest::Error,
    },
    #[error(
        "healthcheck.{field} duration {duration:?} must be positive and fit the runtime timer"
    )]
    InvalidDuration {
        field: &'static str,
        duration: Duration,
    },
    #[error("service did not become ready within {timeout:?}")]
    ReadinessTimeout {
        timeout: Duration,
        #[source]
        last_failure: Option<ProbeFailure>,
    },
}

#[derive(Debug, Error)]
pub enum ProbeFailure {
    #[error("script probe failed: {message}")]
    Script { message: String },
    #[error("probe timed out after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error("TCP connection failed: {source}")]
    Tcp {
        #[source]
        source: io::Error,
    },
    #[error("Unix socket connection failed: {source}")]
    Socket {
        #[source]
        source: io::Error,
    },
    #[error("HTTP request failed: {source}")]
    Http {
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP probe returned {status}")]
    HttpStatus { status: StatusCode },
}

#[derive(Debug)]
pub enum ProbeResult {
    Healthy,
    Unhealthy(ProbeFailure),
}

impl ProbeResult {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

#[derive(Debug, Clone)]
enum Probe {
    Script { config: Box<ServiceConfig> },
    Tcp { host: String, port: u16 },
    Socket { path: std::path::PathBuf },
    Http { client: Client, url: Url },
}

/// A validated probe snapshot. Runtime failures are results, rather than setup
/// errors. HTTP probes inspect headers; script probes inspect exit status only.
#[derive(Debug, Clone)]
pub struct HealthChecker {
    probe: Probe,
    timeout: Duration,
    interval: Duration,
    retries: u32,
}

impl HealthChecker {
    pub fn new(config: &HealthCheck) -> Result<Self, HealthCheckError> {
        config.validate()?;
        if cfg!(windows) && matches!(config, HealthCheck::Socket { .. }) {
            return Err(HealthCheckError::UnsupportedSocket);
        }
        let (interval, timeout, retries) = match config {
            HealthCheck::Tcp {
                interval,
                timeout,
                retries,
                ..
            }
            | HealthCheck::Http {
                interval,
                timeout,
                retries,
                ..
            } => (*interval, *timeout, *retries),
            HealthCheck::Socket {
                interval,
                timeout,
                retries,
                ..
            }
            | HealthCheck::Script {
                interval,
                timeout,
                retries,
                ..
            } => (*interval, *timeout, *retries),
        };
        checked_deadline(interval, "interval")?;
        checked_deadline(timeout, "timeout")?;
        let probe = match config {
            HealthCheck::Script { command, .. } => Probe::Script {
                config: Box::new(ServiceConfig {
                    command: command.clone(),
                    listen: Vec::new(),
                    ports: Default::default(),
                    paths: Default::default(),
                    cwd: None,
                    requires: Vec::new(),
                    monitor_requires: false,
                    env: Default::default(),
                    env_file: None,
                    depends_on: Vec::new(),
                    restart_on_dep_recovery: false,
                    healthcheck: None,
                    restart: Default::default(),
                    limits: None,
                }),
            },
            HealthCheck::Tcp { host, port, .. } => Probe::Tcp {
                host: host.clone(),
                port: *port,
            },
            HealthCheck::Http { url, .. } => {
                let client = Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(timeout)
                    .build()
                    .map_err(|source| HealthCheckError::Client { source })?;
                let url = Url::parse(url).expect("HTTP URL was validated above");
                Probe::Http { client, url }
            }
            HealthCheck::Socket { path, .. } => Probe::Socket { path: path.clone() },
        };
        Ok(Self {
            probe,
            timeout,
            interval,
            retries,
        })
    }

    /// Resolve socket paths and script execution context against the service.
    /// Environment files retain the same runtime precedence as service spawn.
    pub fn for_service(
        config: &HealthCheck,
        service: &ServiceConfig,
    ) -> Result<Self, HealthCheckError> {
        let mut checker = Self::new(config)?;
        match &mut checker.probe {
            Probe::Socket { path } if path.is_relative() => {
                *path = service
                    .cwd
                    .as_deref()
                    .unwrap_or(std::path::Path::new("."))
                    .join(&*path);
            }
            Probe::Script { config } => {
                config.cwd = service.cwd.clone();
                config.env = service.env.clone();
                config.env_file = service.env_file.clone();
            }
            _ => {}
        }
        Ok(checker)
    }

    /// Perform one bounded probe, including DNS, HTTP/TLS, or script environment
    /// loading and process execution. Cancellation closes network requests or
    /// terminates the script's process group/Job; Tokio reaps its leader.
    pub async fn probe(&self) -> ProbeResult {
        let probe = async {
            match &self.probe {
                Probe::Script { config } => {
                    let mut process = super::process_manager::ManagedProcess::spawn_probe(config)
                        .await
                        .map_err(|error| ProbeFailure::Script {
                            message: error.to_string(),
                        })?;
                    let status = process.wait().await.map_err(|error| ProbeFailure::Script {
                        message: error.to_string(),
                    })?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(ProbeFailure::Script {
                            message: format!("command exited with {status}"),
                        })
                    }
                }
                Probe::Tcp { host, port } => TcpStream::connect((host.as_str(), *port))
                    .await
                    .map(|_| ())
                    .map_err(|source| ProbeFailure::Tcp { source }),
                Probe::Socket { path } => {
                    #[cfg(unix)]
                    {
                        tokio::net::UnixStream::connect(path)
                            .await
                            .map(|_| ())
                            .map_err(|source| ProbeFailure::Socket { source })
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = path;
                        Err(ProbeFailure::Socket {
                            source: io::Error::new(
                                io::ErrorKind::Unsupported,
                                "Unix sockets are not supported on this platform",
                            ),
                        })
                    }
                }
                Probe::Http { client, url } => {
                    let response = client.get(url.clone()).send().await.map_err(|source| {
                        if source.is_timeout() {
                            ProbeFailure::Timeout {
                                timeout: self.timeout,
                            }
                        } else {
                            ProbeFailure::Http { source }
                        }
                    })?;
                    if response.status().is_success() {
                        Ok(())
                    } else {
                        Err(ProbeFailure::HttpStatus {
                            status: response.status(),
                        })
                    }
                }
            }
        };
        bounded_probe(probe, self.timeout).await
    }

    /// Poll until the first success, bounded by an overall readiness deadline.
    /// A readiness deadline is independent of the per-probe timeout and monitoring
    /// failure threshold. Timeout preserves the last completed probe failure.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<(), HealthCheckError> {
        let deadline = checked_deadline(timeout, "readiness-timeout")?;
        let mut ticker = polling_interval(self.interval);
        let mut last_failure = None;
        let wait = async {
            loop {
                ticker.tick().await;
                match self.probe().await {
                    ProbeResult::Healthy => return,
                    ProbeResult::Unhealthy(failure) => last_failure = Some(failure),
                }
            }
        };
        match tokio::time::timeout_at(deadline, wait).await {
            Ok(()) => Ok(()),
            Err(_) => Err(HealthCheckError::ReadinessTimeout {
                timeout,
                last_failure,
            }),
        }
    }
}

fn checked_deadline(duration: Duration, field: &'static str) -> Result<Instant, HealthCheckError> {
    if !duration.is_zero() {
        if let Some(deadline) = Instant::now().checked_add(duration) {
            return Ok(deadline);
        }
    }
    Err(HealthCheckError::InvalidDuration { field, duration })
}

fn polling_interval(interval: Duration) -> Interval {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

async fn bounded_probe(
    probe: impl Future<Output = Result<(), ProbeFailure>>,
    timeout: Duration,
) -> ProbeResult {
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(())) => ProbeResult::Healthy,
        Ok(Err(failure)) => ProbeResult::Unhealthy(failure),
        Err(_) => ProbeResult::Unhealthy(ProbeFailure::Timeout { timeout }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    Retrying,
    Unhealthy,
}

#[derive(Debug)]
pub struct HealthObservation {
    pub result: ProbeResult,
    pub consecutive_failures: u32,
    pub state: HealthState,
}

/// Serial polling with an immediate first probe and skipped missed ticks.
/// Failure counts update only after a completed probe and reset after success.
/// The caller owns cancellation and any restart or readiness policy.
#[derive(Debug)]
pub struct HealthMonitor {
    checker: HealthChecker,
    ticker: Interval,
    consecutive_failures: u32,
}

impl HealthMonitor {
    /// Create a monitor inside a Tokio runtime with its time driver enabled.
    pub fn new(checker: HealthChecker) -> Self {
        let ticker = polling_interval(checker.interval);
        Self {
            checker,
            ticker,
            consecutive_failures: 0,
        }
    }

    /// Wait for a tick and report one probe. This method never overlaps probes.
    pub async fn next_check(&mut self) -> HealthObservation {
        self.ticker.tick().await;
        let result = self.checker.probe().await;
        self.record(result)
    }

    fn record(&mut self, result: ProbeResult) -> HealthObservation {
        let state = if result.is_healthy() {
            self.consecutive_failures = 0;
            HealthState::Healthy
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            if self.consecutive_failures >= self.checker.retries {
                HealthState::Unhealthy
            } else {
                HealthState::Retrying
            }
        };
        HealthObservation {
            result,
            consecutive_failures: self.consecutive_failures,
            state,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};

    use super::*;

    fn tcp_config() -> HealthCheck {
        HealthCheck::Tcp {
            host: "127.0.0.1".into(),
            port: 80,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            retries: 3,
        }
    }

    fn failure() -> ProbeResult {
        ProbeResult::Unhealthy(ProbeFailure::Tcp {
            source: io::Error::from(io::ErrorKind::ConnectionRefused),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn test_health_timeout_bounds_a_stalled_tcp_operation() {
        let timeout = Duration::from_secs(2);
        let start = Instant::now();
        let result = bounded_probe(pending(), timeout).await;
        assert!(
            matches!(result, ProbeResult::Unhealthy(ProbeFailure::Timeout { timeout: actual }) if actual == timeout)
        );
        assert_eq!(Instant::now() - start, timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn test_health_immediate_probe_results_do_not_wait_for_timeout() {
        let start = Instant::now();
        assert!(bounded_probe(ready(Ok(())), Duration::from_secs(2))
            .await
            .is_healthy());
        assert!(matches!(
            bounded_probe(
                ready(Err(ProbeFailure::Tcp {
                    source: io::Error::from(io::ErrorKind::ConnectionRefused)
                })),
                Duration::from_secs(2)
            )
            .await,
            ProbeResult::Unhealthy(ProbeFailure::Tcp { .. })
        ));
        assert_eq!(Instant::now(), start);
    }

    #[test]
    fn test_health_setup_validates_direct_configs_without_network_access() {
        let cases = [
            ("type: tcp\nport: 0", "healthcheck.port"),
            ("type: tcp\nport: 80\nhost: ''", "healthcheck.host"),
            ("type: http\nurl: '/health'", "healthcheck.url"),
            ("type: http\nurl: 'ftp://localhost/'", "healthcheck.url"),
            ("type: tcp\nport: 80\ninterval: 0s", "healthcheck.interval"),
            ("type: tcp\nport: 80\ntimeout: 0s", "healthcheck.timeout"),
            ("type: tcp\nport: 80\nretries: 0", "healthcheck.retries"),
        ];
        for (yaml, expected) in cases {
            let config = serde_yaml::from_str(yaml).unwrap();
            assert!(matches!(HealthChecker::new(&config),
                Err(HealthCheckError::InvalidConfig(ConfigValidationError::InvalidField { field, .. })) if field == expected));
        }
        let config = serde_yaml::from_str("type: socket\npath: /tmp/db.sock").unwrap();
        assert_eq!(HealthChecker::new(&config).is_ok(), cfg!(unix));
    }

    #[test]
    fn test_health_rejects_overflowing_probe_and_poll_durations() {
        for field in ["interval", "timeout"] {
            let mut config = tcp_config();
            let HealthCheck::Tcp {
                interval, timeout, ..
            } = &mut config
            else {
                unreachable!()
            };
            *if field == "interval" {
                interval
            } else {
                timeout
            } = Duration::MAX;
            assert!(
                matches!(HealthChecker::new(&config), Err(HealthCheckError::InvalidDuration { field: actual, .. }) if actual == field)
            );
        }
    }

    #[tokio::test]
    async fn test_health_monitor_threshold_reset_and_saturation() {
        let checker = HealthChecker::new(&tcp_config()).unwrap();
        let mut monitor = HealthMonitor::new(checker);
        for (count, state) in [
            (1, HealthState::Retrying),
            (2, HealthState::Retrying),
            (3, HealthState::Unhealthy),
            (4, HealthState::Unhealthy),
        ] {
            let observation = monitor.record(failure());
            assert_eq!(
                (observation.consecutive_failures, observation.state),
                (count, state)
            );
        }
        let recovered = monitor.record(ProbeResult::Healthy);
        assert_eq!(
            (recovered.consecutive_failures, recovered.state),
            (0, HealthState::Healthy)
        );
        assert_eq!(monitor.record(failure()).state, HealthState::Retrying);
        monitor.consecutive_failures = u32::MAX;
        let saturated = monitor.record(failure());
        assert_eq!(saturated.consecutive_failures, u32::MAX);
        assert_eq!(saturated.state, HealthState::Unhealthy);
    }

    #[tokio::test(start_paused = true)]
    async fn test_health_polling_interval_skips_missed_ticks() {
        let period = Duration::from_secs(1);
        let mut ticker = polling_interval(period);
        let start = Instant::now();
        ticker.tick().await;
        assert_eq!(Instant::now(), start);
        tokio::time::advance(Duration::from_millis(3500)).await;
        ticker.tick().await;
        assert_eq!(Instant::now() - start, Duration::from_millis(3500));
        ticker.tick().await;
        assert_eq!(Instant::now() - start, Duration::from_secs(4));
    }

    #[tokio::test]
    async fn test_health_readiness_rejects_zero_or_overflowing_deadlines() {
        let checker = HealthChecker::new(&tcp_config()).unwrap();
        for duration in [Duration::ZERO, Duration::MAX] {
            assert!(matches!(
                checker.wait_ready(duration).await,
                Err(HealthCheckError::InvalidDuration {
                    field: "readiness-timeout",
                    ..
                })
            ));
        }
    }
}
