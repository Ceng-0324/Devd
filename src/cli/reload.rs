use std::{fmt::Write, io::Read, path::Path, sync::Arc};

use anyhow::{bail, Context, Result};
use tokio::sync::watch;

use super::protocol::{self, Request, Response};
use crate::{
    config::{ConfigLoader, DevdConfig},
    core::{reload::ReloadPlan, service_manager::RuntimeSnapshot},
};

const MAX_CANDIDATE_BYTES: u64 = 1024 * 1024;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Required: inspect changes without applying them.
    #[arg(long, required = true)]
    dry_run: bool,
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
    let Response::ReloadPlan(plan) =
        protocol::request(socket, Request::PreviewReload { candidate }).await?
    else {
        bail!("unexpected reload preview response");
    };
    super::output(&render(&plan, args.json)?)
}

/// Read only one bounded, regular YAML file. The blocking task owns no control,
/// state writer or event recorder, so cancellation cannot produce side effects.
pub(super) async fn preview(
    candidate: std::path::PathBuf,
    base: Arc<DevdConfig>,
    profile: Option<String>,
    snapshots: watch::Receiver<RuntimeSnapshot>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<ReloadPlan> {
    if !candidate.is_absolute() {
        bail!("candidate configuration path must be absolute");
    }
    tokio::task::spawn_blocking(move || {
        // Keep the concurrency slot until an uncancellable OS read finishes.
        let _permit = permit;
        let path =
            std::fs::canonicalize(&candidate).context("cannot locate candidate configuration")?;
        let file = crate::platform::files::open_regular(&path, false, false)
            .context("candidate configuration must be a readable regular file")?;
        let mut contents = String::new();
        file.take(MAX_CANDIDATE_BYTES + 1)
            .read_to_string(&mut contents)
            .context("cannot read candidate YAML as UTF-8")?;
        if contents.len() as u64 > MAX_CANDIDATE_BYTES {
            bail!("candidate configuration exceeds 1 MiB");
        }
        let mut config = ConfigLoader::from_str_profile(&contents, &path, profile.as_deref())?;
        super::resolve_working_directories(&mut config, &path);
        let snapshot = snapshots.borrow().clone();
        crate::core::reload::preview(&base, &config, &snapshot, profile.as_deref())
    })
    .await
    .context("configuration preview task failed")?
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
