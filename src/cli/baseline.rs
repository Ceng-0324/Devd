//! Explicit local observations. Imported selectors are resolved through current YAML.
mod model;
mod resources;
#[cfg(test)]
mod tests;

use super::export::output::{self, CancelOnDrop, Control, DEADLINE};
use crate::{config::ConfigLoader, core::environment_baseline as drift};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use model::*;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(clap::Args)]
pub(super) struct Args {
    #[command(subcommand)]
    action: Action,
}
#[derive(clap::Subcommand)]
enum Action {
    /// Observe explicitly selected resources and save a new baseline; never overwrite.
    Capture {
        #[arg(long)]
        service: String,
        /// requires:INDEX, paths:NAME, cwd, program or env-file; repeat to select more.
        #[arg(long, required = true)]
        resource: Vec<Selection>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Compare against current on-disk declarations; no supervisor or process control.
    Check {
        baseline: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

pub(super) async fn run(
    args: Args,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let config = config.to_owned();
    let state = state.to_owned();
    let profile = profile.map(str::to_owned);
    let c = control.clone();
    let work = tokio::task::spawn_blocking(move || prepare(args, &config, &state, profile, &c));
    let (bytes, outcome) = tokio::select! {
        biased;
        _ = signals.recv() => bail!("baseline operation cancelled; an accepted publication may have completed"),
        _ = tokio::time::sleep(DEADLINE) => bail!("baseline operation exceeded 30s; an accepted publication may have completed"),
        result = work => result.context("baseline worker failed")??,
    };
    use tokio::io::AsyncWriteExt;
    let mut stdout = super::stdout::Stdout::new()?;
    tokio::select! {
        biased;
        _ = signals.recv() => bail!("baseline output cancelled; output may be incomplete"),
        result = tokio::time::timeout(super::protocol::IO_TIMEOUT, async {
            stdout.write_all(&bytes).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await
        }) => result.context("baseline output timed out")??,
    }
    if outcome != drift::Outcome::NoObservedChange {
        bail!("baseline comparison reports changes or unknown evidence");
    }
    Ok(())
}

fn prepare(
    args: Args,
    path: &Path,
    state: &Path,
    profile: Option<String>,
    c: &Control,
) -> Result<(Vec<u8>, drift::Outcome)> {
    c.check()?;
    let imported = match &args.action {
        Action::Check { baseline, .. } => {
            let bytes = output::read_regular(baseline, c, MAX_BYTES)?.bytes;
            let b: Baseline = serde_json::from_slice(&bytes).context("invalid baseline JSON")?;
            b.validate()?;
            Some(b)
        }
        _ => None,
    };
    let config_bytes = output::read_regular(path, c, 1024 * 1024)?.bytes;
    let yaml = std::str::from_utf8(&config_bytes).context("configuration is not UTF-8")?;
    let mut config = ConfigLoader::from_str_profile(yaml, path, profile.as_deref())?;
    config.validate()?;
    super::resolve_working_directories(&mut config, path);
    resources::validate_graph(&config)?;
    c.check()?;
    match args.action {
        Action::Capture {
            service,
            resource,
            output: destination,
        } => {
            if resource.len() > MAX_RESOURCES {
                bail!("at most {MAX_RESOURCES} resources may be selected");
            }
            let mut unique = BTreeSet::new();
            for selection in &resource {
                if !unique.insert(selection) {
                    bail!("duplicate resource selection");
                }
                resources::resolve(&config, state, &service, selection)?;
            }
            let scope = model::local_scope(state, true, c)?;
            let from = Utc::now();
            let mut entries = Vec::new();
            for selection in resource {
                c.check()?;
                let resolved = resources::resolve(&config, state, &service, &selection)?;
                let evidence =
                    crate::core::resource_inspection::inspect_evidence(&resolved.requested_path);
                entries.push(Entry {
                    selection,
                    declaration: resolved,
                    evidence,
                });
            }
            let baseline = Baseline {
                schema_version: 1,
                source: "explicit-effective-disk-config".into(),
                id: uuid::Uuid::new_v4().to_string(),
                context: ContextKey {
                    config: path.to_owned(),
                    state: state.to_owned(),
                    profile,
                },
                local_scope: scope.context("local baseline provenance is unavailable")?,
                captured_from: from,
                captured_to: Utc::now(),
                service,
                entries,
            };
            baseline.validate()?;
            if output::encode(&baseline)?.len() > MAX_BYTES {
                bail!("baseline exceeds 2 MiB");
            }
            let destination = std::path::absolute(destination)?;
            output::publish_report(&destination, &baseline, c)?;
            Ok((format!("Saved explicit baseline to {destination:?}; no resources or processes modified.").into_bytes(), drift::Outcome::NoObservedChange))
        }
        Action::Check {
            baseline: input,
            json,
        } => {
            let baseline = imported.expect("check loaded baseline");
            let from = Utc::now();
            let context = ContextKey {
                config: path.to_owned(),
                state: state.to_owned(),
                profile,
            };
            let local = model::local_scope(state, false, c)?;
            let comparable =
                baseline.context == context && local.as_ref() == Some(&baseline.local_scope);
            let mut entries = Vec::new();
            let mut cache = resources::Cache::default();
            for old in &baseline.entries {
                c.check()?;
                let selected =
                    resources::resolve(&config, state, &baseline.service, &old.selection);
                let (current, comparison, impact) = match selected {
                    Ok(declaration) => {
                        // Never inspect paths from imported evidence, even after declaration changes.
                        let evidence = cache
                            .observe(&declaration.requested_path, c)?
                            .context("resource inspection capacity exhausted")?;
                        let comparison = if !comparable {
                            drift::unknown("local-context-not-comparable")
                        } else if declaration != old.declaration {
                            drift::unknown("resource-declaration-changed")
                        } else {
                            drift::compare(&old.evidence, &evidence)
                        };
                        let impact = resources::impact(
                            &config,
                            state,
                            &baseline.service,
                            &old.selection,
                            &declaration,
                            &evidence,
                            &mut cache,
                            c,
                        )?;
                        (
                            Some(Current {
                                declaration,
                                evidence,
                            }),
                            comparison,
                            Some(impact),
                        )
                    }
                    Err(_) => (
                        None,
                        drift::unknown("resource-declaration-unavailable"),
                        None,
                    ),
                };
                entries.push(CheckEntry {
                    recorded: old.clone(),
                    selection: old.selection.clone(),
                    comparison,
                    current,
                    current_declared_impact: impact,
                });
            }
            let outcome = if entries
                .iter()
                .any(|e| e.comparison.outcome == drift::Outcome::Changed)
            {
                drift::Outcome::Changed
            } else if entries
                .iter()
                .any(|e| e.comparison.outcome == drift::Outcome::Unknown)
            {
                drift::Outcome::Unknown
            } else {
                drift::Outcome::NoObservedChange
            };
            let report = CheckReport {
                schema_version: 1,
                source: "current-effective-disk-config",
                baseline_file: input,
                baseline_id: baseline.id,
                baseline_captured_from: baseline.captured_from,
                baseline_captured_to: baseline.captured_to,
                captured_from: from,
                captured_to: Utc::now(),
                service: baseline.service,
                context,
                local_context_comparable: comparable,
                outcome,
                entries,
                limitations: vec![
                    "metadata-is-not-content-equality",
                    "non-atomic-observations",
                    "current-declared-impact-not-historical-causality",
                    "on-disk-config-not-supervisor-config",
                    "provenance-is-local-not-authentication",
                    "windows-file-identity-not-recorded",
                ],
            };
            c.check()?;
            let bytes = if json {
                output::encode(&report)?
            } else {
                render(&report).into_bytes()
            };
            Ok((bytes, outcome))
        }
    }
}

fn render(r: &CheckReport) -> String {
    let mut text = format!("Environment baseline {:?}: {:?}\nCurrent on-disk declarations; non-atomic metadata evidence.\n", r.baseline_id, r.outcome);
    for entry in &r.entries {
        text.push_str(&format!(
            "  {:?}: {:?} {:?}\n",
            entry.selection, entry.comparison.outcome, entry.comparison.reasons
        ));
        if let Some(impact) = &entry.current_declared_impact {
            text.push_str(&format!(
                "    current declared impact (complete={}): {:?}\n",
                impact.complete, impact.services
            ));
        }
    }
    text.push_str("Metadata does not prove content equality or fault causality. No recovery or restart performed.\n");
    text
}
