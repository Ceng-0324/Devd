use std::{fmt::Write, io::Read, path::Path, sync::Arc};

use anyhow::{bail, Context, Result};
use tokio::sync::watch;

use super::protocol::{self, Request, Response};
use crate::{
    config::{ConfigLoader, DevdConfig},
    core::{
        reload::{ReloadOutcome, ReloadPlan},
        service_manager::RuntimeSnapshot,
    },
};

const MAX_CANDIDATE_BYTES: u64 = 1024 * 1024;

#[derive(clap::Args)]
#[group(skip)]
#[command(group(clap::ArgGroup::new("mode").args(["dry_run", "apply"]).required(true)))]
pub(super) struct Args {
    /// Inspect changes without applying them.
    #[arg(long)]
    dry_run: bool,
    /// Apply the verified plan; failures stop the whole stack without rollback.
    #[arg(long, requires = "plan")]
    apply: bool,
    /// Plan ID from --dry-run; stale plans are rejected before stopping services.
    #[arg(long, requires = "apply", value_parser = plan_id)]
    plan: Option<String>,
    /// Candidate YAML; defaults to --config. Relative cwd uses this file's directory.
    #[arg(long)]
    candidate: Option<std::path::PathBuf>,
    /// Print the versioned report as JSON.
    #[arg(long)]
    json: bool,
}

pub(super) async fn run(args: Args, socket: &Path, config_path: &Path) -> Result<()> {
    let candidate =
        super::absolute_config(args.candidate.as_deref().unwrap_or(config_path)).await?;
    let request = if args.apply {
        Request::ApplyReload {
            candidate,
            plan_id: args.plan.context("--apply requires --plan")?,
        }
    } else {
        Request::PreviewReload { candidate }
    };
    match protocol::request(socket, request).await? {
        Response::ReloadPlan(plan) => super::output(&render(&plan, args.json)?),
        Response::Reloaded(report) => {
            let text = if args.json {
                format!("{}\n", serde_json::to_string_pretty(&report)?)
            } else {
                format!("Configuration reload: {:?}\nPlan: {}\nConfig committed: {}\nStopped: {}\nStarted: {}\nReady: {}\n{}",
                report.outcome, report.plan_id, report.config_committed,
                report.stopped.join(", "), report.started.join(", "), report.ready.join(", "),
                report.failure.as_ref().map_or(String::new(), |error| format!("Failure: {error}\n")))
            };
            super::output(&text)?;
            if report.outcome != ReloadOutcome::Applied {
                bail!("configuration reload did not complete; inspect reported progress and lifecycle events");
            }
            Ok(())
        }
        _ => bail!("unexpected configuration reload response"),
    }
}

pub(super) fn plan_id(value: &str) -> Result<String, String> {
    if value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        Ok(value.to_owned())
    } else {
        Err("plan ID must be sha256:<64 hexadecimal digits> from --dry-run".into())
    }
}

/// Read only one bounded, regular YAML file. The blocking task owns no control,
/// state writer or event recorder, so cancellation cannot produce side effects.
pub(super) async fn preview(
    candidate: std::path::PathBuf,
    configurations: watch::Receiver<Arc<DevdConfig>>,
    profile: Option<String>,
    snapshots: watch::Receiver<RuntimeSnapshot>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<ReloadPlan> {
    tokio::task::spawn_blocking(move || {
        // Keep the concurrency slot until an uncancellable OS read finishes.
        let _permit = permit;
        let config = read_candidate(&candidate, profile.as_deref())?;
        let snapshot = snapshots.borrow();
        let base = configurations.borrow();
        crate::core::reload::preview(&base, &config, &snapshot, profile.as_deref())
    })
    .await
    .context("configuration preview task failed")?
}

pub(super) async fn load_candidate(
    candidate: std::path::PathBuf,
    profile: Option<String>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<DevdConfig> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let config = read_candidate(&candidate, profile.as_deref())?;
        crate::core::service_manager::prepare_config(&config, Path::new("."))?;
        Ok(config)
    })
    .await
    .context("candidate configuration task failed")?
}

/// Read a candidate on a blocking worker before any process control is possible.
fn read_candidate(candidate: &Path, profile: Option<&str>) -> Result<DevdConfig> {
    if !candidate.is_absolute() {
        bail!("candidate configuration path must be absolute");
    }
    let path = std::fs::canonicalize(candidate).context("cannot locate candidate configuration")?;
    let file = crate::platform::files::open_regular(&path, false, false)
        .context("candidate configuration must be a readable regular file")?;
    let mut contents = String::new();
    file.take(MAX_CANDIDATE_BYTES + 1)
        .read_to_string(&mut contents)
        .context("cannot read candidate YAML as UTF-8")?;
    if contents.len() as u64 > MAX_CANDIDATE_BYTES {
        bail!("candidate configuration exceeds 1 MiB");
    }
    let mut config = ConfigLoader::from_str_profile(&contents, &path, profile)?;
    super::resolve_working_directories(&mut config, &path);
    Ok(config)
}

fn render(plan: &ReloadPlan, json: bool) -> Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(plan)?));
    }
    let mut text = format!("Configuration impact preview (no changes applied)\nRun: {}  Profile: {}\nBase: {}\nCandidate: {}\nPlan: {}\n",
        plan.run_id, plan.profile.as_deref().unwrap_or("(base)"), plan.base_config_id, plan.candidate_config_id, plan.plan_id);
    for (name, impact) in &plan.services {
        writeln!(
            text,
            "  {name}: {} fields=[{}] affected-by=[{}]",
            impact.change,
            impact.changed_fields.join(", "),
            impact.affected_by.join(", ")
        )?;
    }
    for (label, layers) in [
        ("Expected stop layers", &plan.stop_layers),
        ("Expected start layers", &plan.start_layers),
    ] {
        writeln!(text, "{label}:")?;
        if layers.is_empty() {
            text.push_str("  (none)\n");
        }
        for (index, layer) in layers.iter().enumerate() {
            writeln!(text, "  {}: {}", index + 1, layer.join(", "))?;
        }
    }
    for reason in &plan.unsupported_changes {
        writeln!(text, "Unsupported: {reason}")?;
    }
    for limitation in &plan.limitations {
        writeln!(text, "Note: {limitation}")?;
    }
    Ok(text)
}
