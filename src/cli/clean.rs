use std::{fmt::Write, io::Read, path::Path};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::{
    config::ConfigLoader,
    core::owned_paths::{self, Output},
};

#[derive(clap::Args)]
#[group(skip)]
#[command(group(clap::ArgGroup::new("mode").args(["dry_run", "apply"]).required(true)))]
pub(super) struct Args {
    /// Preview explicitly authorized, registered directories; deletes nothing.
    #[arg(long)]
    dry_run: bool,
    /// Remove only the directories in a fresh matching plan, after shutdown.
    #[arg(long, requires = "plan")]
    apply: bool,
    /// Plan ID from --dry-run. Changed config, run or directory trees invalidate it.
    #[arg(long, requires = "apply", value_parser = super::reload::plan_id)]
    plan: Option<String>,
    /// Print the versioned plan or execution report as JSON.
    #[arg(long)]
    json: bool,
}

pub(super) async fn run(
    args: Args,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let config = config.to_path_buf();
    let state = state.to_path_buf();
    let profile = profile.map(str::to_owned);
    let result = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        crate::platform::files::open_regular(&config, false, true)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            bail!("cleanup configuration exceeds 1 MiB");
        }
        let fingerprint = format!("sha256:{:x}", Sha256::digest(&bytes));
        let configuration = ConfigLoader::from_str_profile(
            std::str::from_utf8(&bytes)?,
            &config,
            profile.as_deref(),
        )?;
        configuration.validate()?;
        owned_paths::clean(
            &configuration,
            &config,
            &fingerprint,
            &state,
            profile.as_deref(),
            args.plan.as_deref(),
        )
    })
    .await
    .context("cleanup task failed")??;
    let (text, failed) = match result {
        Output::Preview(plan) => {
            let text = if args.json {
                format!("{}\n", serde_json::to_string_pretty(&plan)?)
            } else {
                let mut text = format!(
                    "Cleanup preview (no changes)\nPlan: {}\nRun: {}\n",
                    plan.plan_id, plan.run_id
                );
                for resource in &plan.resources {
                    writeln!(
                        text,
                        "  {}.{}: {} ({} entries, {} bytes, {})",
                        resource.service,
                        resource.name,
                        serde_json::to_string(&resource.path)?,
                        resource.entries,
                        resource.bytes,
                        if resource.present {
                            "remove owned directory"
                        } else {
                            "already absent; retire record"
                        }
                    )?;
                }
                for path in &plan.retained {
                    writeln!(
                        text,
                        "  Retain (no current authorization or creation record): {}",
                        serde_json::to_string(path)?
                    )?;
                }
                text.push_str("Shared/unregistered paths, logs, events, snapshots and instance metadata are preserved.\nApply requires: devd clean --apply --plan <ID> (with the same config/profile/state options).\n");
                text
            };
            (text, false)
        }
        Output::Applied(report) => {
            let failed = report.outcome != "applied";
            let text = if args.json {
                format!("{}\n", serde_json::to_string_pretty(&report)?)
            } else {
                let mut text = format!(
                    "Cleanup: {}\nPlan: {}\nAlready applied: {}\n",
                    report.outcome, report.plan_id, report.already_applied
                );
                for path in &report.removed {
                    writeln!(text, "  Removed: {}", serde_json::to_string(path)?)?;
                }
                for path in &report.already_absent {
                    writeln!(
                        text,
                        "  Already absent (record retired): {}",
                        serde_json::to_string(path)?
                    )?;
                }
                if let Some(error) = &report.failure {
                    writeln!(text, "Failure: {}", serde_json::to_string(error)?)?;
                }
                text
            };
            (text, failed)
        }
    };
    super::output(&text)?;
    if failed {
        bail!("cleanup did not complete; inspect the report and preview again");
    }
    Ok(())
}
