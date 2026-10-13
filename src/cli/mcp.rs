//! MCP owns protocol adaptation; control_session owns run binding and permissions.
mod tools;
mod transport;

use super::control_session::{self, Grant, Session};
use anyhow::{Context, Result};
use rmcp::{model::*, service::RequestContext, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Exchange MCP JSON-RPC messages on stdin/stdout.
    #[arg(long, required = true)]
    stdio: bool,
    /// Explicit controls; omitted means read-only. Grants cannot come from tools.
    #[arg(long, value_enum, value_delimiter = ',', conflicts_with = "report")]
    allow: Vec<Grant>,
    /// Load only this historical report; never attach to a running instance.
    #[arg(long)]
    pub(super) report: Option<PathBuf>,
}

enum Source {
    Live(Box<Session>),
    Report(Value),
}
struct Adapter {
    source: Source,
    controls: Semaphore,
    admissions: transport::Admissions,
    shutdown: CancellationToken,
}

impl ServerHandler for Adapter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_server_info(Implementation::new("devd", env!("CARGO_PKG_VERSION")))
            .with_instructions("Local devd session. Live tools are bound to one instance and run; report tools contain historical evidence only. Controls require launcher grants and an explicit target from devd_identity. Reload/clean require fresh preview plans. Cancellation does not roll back accepted actions; inspect status/events before retrying. Report availability is not proof that its outcome succeeded. No shell or automatic recovery is provided.")
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ])
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.is_some_and(|r| r.cursor.is_some()) {
            return Err(ErrorData::invalid_params(
                "this tool catalog has no pages",
                None,
            ));
        }
        Ok(ListToolsResult::with_all_items(tools::catalog(matches!(
            self.source,
            Source::Report(_)
        ))))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // Cancellation can suppress the SDK response. Retire its admission only
        // when this handler has actually stopped, never on receipt of cancel.
        let _retire = self.admissions.completion_guard(context.id.clone());
        let work = async {
            let args = Value::Object(request.arguments.unwrap_or_default());
            match &self.source {
                Source::Report(report) => {
                    if request.name != "devd_diagnose" {
                        return Err(ErrorData::invalid_params("unknown offline tool", None));
                    }
                    tools::empty(args)?;
                    Ok(tools::result(report.clone()))
                }
                Source::Live(session) => {
                    let operation = tools::operation(&request.name, args)?;
                    let _permit = if operation.control().is_some() {
                        match self.controls.try_acquire() {
                            Ok(permit) => Some(permit),
                            Err(_) => return Ok(tools::failure(
                                "busy",
                                "another control is in progress; inspect status before retrying",
                            )),
                        }
                    } else {
                        None
                    };
                    let result = session.execute(operation).await;
                    Ok(match result {
                        Ok(value) => tools::result(value),
                        Err(error) => tools::failure(&error.code, &error.message),
                    })
                }
            }
        };
        let deadline = if request.name == "devd_wait" {
            Duration::from_secs(3605)
        } else {
            Duration::from_secs(30)
        };
        let result = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => tools::failure("cancelled", "session closed; accepted controls may continue"),
            _ = context.ct.cancelled() => tools::failure("cancelled", "request cancelled; accepted controls may continue"),
            result = tokio::time::timeout(deadline, work) => match result {
                Ok(result) => result?,
                Err(_) => tools::failure("timed-out", "request exceeded its deadline; accepted controls may continue"),
            }
        };
        Ok(result.into())
    }
}

pub(super) async fn run(
    args: Args,
    socket: &Path,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let session = tokio::select! {
        _ = signals.recv() => return Ok(()),
        result = control_session::attach(socket, config, state, profile, args.allow) => result?,
    };
    serve(Source::Live(Box::new(session)), signals).await
}

pub(super) async fn run_offline(report: PathBuf) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let report = tokio::select! {
        _ = signals.recv() => return Ok(()),
        result = super::export::diagnose::load_json(&report) => result?,
    };
    serve(Source::Report(report), signals).await
}

async fn serve(source: Source, mut signals: crate::platform::shutdown::Shutdown) -> Result<()> {
    let shutdown = CancellationToken::new();
    let _cancel = shutdown.clone().drop_guard();
    let transport = transport::Stdio::new(shutdown.clone())?;
    let adapter = Adapter {
        source,
        controls: Semaphore::new(1),
        admissions: transport.admissions.clone(),
        shutdown: shutdown.clone(),
    };
    let service = tokio::select! {
        _ = signals.recv() => return Ok(()),
        result = tokio::time::timeout(Duration::from_secs(30), adapter.serve_with_ct(transport, shutdown.clone())) => result.context("MCP initialization exceeded 30s")??,
    };
    tokio::select! {
        _ = signals.recv() => { shutdown.cancel(); },
        result = service.waiting() => { result?; },
    }
    Ok(())
}
