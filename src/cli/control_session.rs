//! Run-bound use cases shared by the JSON-lines and MCP adapters.
use super::{
    instances::Identity,
    protocol::{self, Request, Response},
};
use crate::core::{events::query::EventQuery, owned_paths, readiness::Outcome};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
const MAX_LINE: usize = 16 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Grant {
    Restart,
    Stop,
    Reload,
    Clean,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    pub instance_id: String,
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Operation {
    Describe {},
    Identity {},
    Status {},
    Events {
        query: EventQuery,
    },
    Explain {
        service: String,
    },
    Wait {
        #[serde(default)]
        services: Vec<String>,
        timeout_ms: u64,
    },
    Export {
        #[serde(default)]
        include_logs: bool,
    },
    Incidents {
        #[serde(default)]
        id: Option<String>,
    },
    ReloadPreview {
        candidate: Option<PathBuf>,
    },
    CleanPreview {},
    Restart {
        target: Target,
        service: String,
    },
    Stop {
        target: Target,
    },
    ReloadApply {
        target: Target,
        candidate: Option<PathBuf>,
        plan_id: String,
    },
    CleanApply {
        target: Target,
        plan_id: String,
    },
}

impl Operation {
    pub(super) fn control(&self) -> Option<(Grant, &Target)> {
        match self {
            Self::Restart { target, .. } => Some((Grant::Restart, target)),
            Self::Stop { target } => Some((Grant::Stop, target)),
            Self::ReloadApply { target, .. } => Some((Grant::Reload, target)),
            Self::CleanApply { target, .. } => Some((Grant::Clean, target)),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub(super) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub(super) fn operation(error: anyhow::Error) -> Self {
        Self::new(
            "operation-failed",
            format!(
                "{error:#}; accepted controls may continue: inspect status/events before retrying"
            ),
        )
    }
    pub(super) fn internal(error: serde_json::Error) -> Self {
        Self::new("invalid-response", error.to_string())
    }
}

pub(super) struct Session {
    pub(super) identity: Identity,
    pub(super) socket: PathBuf,
    pub(super) allow: Vec<Grant>,
}

impl Session {
    fn scoped(&self, request: Request) -> Request {
        Request::Scoped {
            instance_id: self.identity.instance_id.clone(),
            run_id: self.identity.run_id.clone(),
            request: Box::new(request),
        }
    }

    pub(super) fn authorize(&self, operation: &Operation) -> Result<(), Failure> {
        if let Some((grant, target)) = operation.control() {
            if !self.allow.contains(&grant) {
                return Err(Failure::new(
                    "permission-denied",
                    "control was not granted by the Agent session launcher",
                ));
            }
            if target.instance_id != self.identity.instance_id {
                return Err(Failure::new(
                    "wrong-instance",
                    "control targets another instance",
                ));
            }
            if target.run_id != self.identity.run_id {
                return Err(Failure::new(
                    "stale-run",
                    "control targets another run; start a new Agent session",
                ));
            }
        }
        Ok(())
    }

    async fn request(&self, request: Request) -> Result<Response, Failure> {
        let request = self.scoped(request);
        if serde_json::to_vec(&request)
            .map_err(Failure::internal)?
            .len()
            > protocol::MAX_REQUEST
        {
            return Err(Failure::new(
                "invalid-request",
                "request exceeds the supervisor's 4096-byte frame limit",
            ));
        }
        match protocol::request(&self.socket, request)
            .await
            .map_err(Failure::operation)?
        {
            Response::ScopeError { code, message } => Err(Failure { code, message }),
            response => Ok(response),
        }
    }

    pub(super) async fn execute(&self, operation: Operation) -> Result<Value, Failure> {
        self.authorize(&operation)?;
        let response = match operation {
            Operation::Describe {} => {
                return Ok(json!({
                    "protocol": "devd-agent", "schema_version": 1,
                    "identity": self.identity, "identity_observation": "session-attachment",
                    "allowed_controls": self.allow,
                    "methods": ["describe", "identity", "status", "events", "explain", "wait", "export", "incidents", "reload-preview", "clean-preview", "restart", "stop", "reload-apply", "clean-apply"],
                    "max_request_bytes": MAX_LINE, "max_response_bytes": MAX_OUTPUT,
                    "scope": "one-instance-and-run", "transport": "json-lines",
                    "request_ids": "correlation-only; controls are not automatically deduplicated",
                    "disconnect": "accepted controls may continue; inspect status/events before retrying"
                }))
            }
            Operation::Identity {} => self.request(Request::Identity).await?,
            Operation::Status {} => self.request(Request::Status).await?,
            Operation::Events { query } => {
                query
                    .validate()
                    .map_err(|e| Failure::new("invalid-request", e))?;
                self.request(Request::Events { query }).await?
            }
            Operation::Explain { service } => self.request(Request::Explain { service }).await?,
            Operation::Export { include_logs } => {
                self.request(Request::Export { include_logs }).await?
            }
            Operation::Incidents { id } => self.request(Request::Incidents { id }).await?,
            Operation::Wait {
                services,
                timeout_ms,
            } => return self.wait(services, timeout_ms).await,
            Operation::Restart { service, .. } => {
                self.request(Request::Restart {
                    service,
                    expected_run_id: Some(self.identity.run_id.clone()),
                })
                .await?
            }
            Operation::Stop { .. } => {
                self.request(Request::Stop {
                    expected_run_id: Some(self.identity.run_id.clone()),
                })
                .await?
            }
            Operation::ReloadPreview { candidate } => {
                let candidate = self.candidate(candidate).await?;
                self.request(Request::PreviewReload { candidate }).await?
            }
            Operation::ReloadApply {
                candidate, plan_id, ..
            } => {
                super::reload::plan_id(&plan_id).map_err(|e| Failure::new("invalid-request", e))?;
                let candidate = self.candidate(candidate).await?;
                self.request(Request::ApplyReload { candidate, plan_id })
                    .await?
            }
            Operation::CleanPreview {} => return self.clean(None).await,
            Operation::CleanApply { plan_id, .. } => {
                super::reload::plan_id(&plan_id).map_err(|e| Failure::new("invalid-request", e))?;
                return self.clean(Some(plan_id)).await;
            }
        };
        match response {
            Response::Identity(identity) => encode(identity),
            Response::Status(snapshot) => {
                let services: BTreeMap<_, _> = snapshot
                    .services
                    .iter()
                    .map(|(name, state)| (name, super::export::SafeServiceState::from(state)))
                    .collect();
                Ok(json!({"observed_at": chrono::Utc::now(), "services": services}))
            }
            Response::Events(batch) => encode(batch),
            Response::Explain(report) => encode(report),
            Response::Export(report) => encode(report),
            Response::Incidents(report) => encode(report),
            Response::ReloadPlan(plan) => encode(plan),
            Response::Reloaded(report) => encode(report),
            Response::Restarted(state) => encode(super::export::SafeServiceState::from(&state)),
            Response::Stopping => Ok(json!({"outcome": "stopping", "completed": false})),
            _ => Err(Failure::new(
                "invalid-response",
                "unexpected supervisor response",
            )),
        }
    }

    async fn candidate(&self, path: Option<PathBuf>) -> Result<PathBuf, Failure> {
        super::absolute_config(path.as_deref().unwrap_or(&self.identity.config))
            .await
            .map_err(Failure::operation)
    }

    async fn clean(&self, plan: Option<String>) -> Result<Value, Failure> {
        // The core verifies this run under the state lock, including completed
        // receipts, so a stop/start between preview and apply cannot switch scope.
        match super::clean::execute(
            &self.identity.config,
            &self.identity.state_dir,
            self.identity.profile.as_deref(),
            plan,
            Some(self.identity.run_id.clone()),
        )
        .await
        {
            Ok(owned_paths::Output::Preview(plan)) => encode(plan),
            Ok(owned_paths::Output::Applied(report)) => encode(report),
            Err(error) if error.is::<owned_paths::StaleRun>() => {
                Err(Failure::new("stale-run", error.to_string()))
            }
            Err(error) => Err(Failure::operation(error)),
        }
    }

    pub(super) async fn wait(
        &self,
        services: Vec<String>,
        timeout_ms: u64,
    ) -> Result<Value, Failure> {
        if !(1..=3_600_000).contains(&timeout_ms) {
            return Err(Failure::new(
                "invalid-request",
                "timeout_ms must be between 1 and 3600000",
            ));
        }
        let request = self.scoped(Request::Wait {
            services,
            timeout_ms,
        });
        if serde_json::to_vec(&request)
            .map_err(Failure::internal)?
            .len()
            > protocol::MAX_REQUEST
        {
            return Err(Failure::new(
                "invalid-request",
                "wait selection exceeds the supervisor frame limit",
            ));
        }
        let started = tokio::time::Instant::now();
        let mut report = super::wait::Report::empty(Outcome::Unavailable);
        report.instance_id = Some(self.identity.instance_id.clone());
        report.run_id = Some(self.identity.run_id.clone());
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            let mut stream = protocol::connect(&self.socket).await?;
            protocol::write(&mut stream, &request).await?;
            super::wait::receive_reports(&mut stream, &mut report).await
        })
        .await;
        match result {
            Err(_) => report.finish(
                Outcome::TimedOut,
                "readiness deadline reached; evidence is the last received observation",
            ),
            Ok(Err(error)) => {
                if let Some(scope) = error.downcast_ref::<protocol::ScopeFailure>() {
                    return Err(Failure::new(&scope.code, &scope.message));
                }
                report.finish(
                    if report.observed_at.is_some() {
                        Outcome::Disconnected
                    } else {
                        Outcome::Unavailable
                    },
                    format!("{error:#}"),
                );
            }
            Ok(Ok(())) => {}
        }
        report.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        encode(report)
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value, Failure> {
    serde_json::to_value(value).map_err(Failure::internal)
}

pub(super) async fn attach(
    socket: &Path,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
    allow: Vec<Grant>,
) -> Result<Session> {
    let Response::Identity(identity) = protocol::request(socket, Request::Identity).await? else {
        bail!("supervisor does not support Agent identity attachment");
    };
    let state = tokio::fs::canonicalize(state).await?;
    if identity.schema_version != 1
        || identity.run_id.is_empty()
        || identity.config != config
        || identity.state_dir != state
        || identity.profile.as_deref() != profile
        || identity.instance_id != super::instances::instance_id(config, &state, profile)?
    {
        bail!("supervisor identity does not match the selected instance");
    }
    Ok(Session {
        identity: *identity,
        socket: socket.into(),
        allow,
    })
}
