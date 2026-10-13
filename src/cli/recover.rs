//! Stopped-instance recovery driver. Never takes ownership of persisted PIDs.
use super::export::output::{self, Control};
use crate::{
    config::ConfigLoader,
    core::{
        owned_paths::recovery::{Plan, Session},
        process_manager::ManagedProcess,
        recovery::{self, Recipe, Report, Versions},
    },
};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;

#[derive(clap::Args)]
#[group(skip)]
#[command(group(clap::ArgGroup::new("mode").args(["dry_run", "apply"]).required(true)))]
pub(super) struct Args {
    #[arg(long)]
    recipe: PathBuf,
    /// Observe a normally stopped instance; execute nothing.
    #[arg(long)]
    dry_run: bool,
    /// Execute a fresh matching recipe plan; commands run with your OS permissions.
    #[arg(long, requires = "plan")]
    apply: bool,
    #[arg(long, requires="apply", value_parser=validate_plan)]
    plan: Option<String>,
    #[arg(long)]
    json: bool,
}
fn validate_plan(value: &str) -> std::result::Result<String, String> {
    recovery::plan_time(value)
        .map(|_| value.to_owned())
        .map_err(|e| e.to_string())
}
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

struct Input {
    config: PathBuf,
    recipe: PathBuf,
    versions: Versions,
}
impl Input {
    fn verify(&self) -> Result<()> {
        let c = Control::new();
        let config = output::read_regular(&self.config, &c, 1024 * 1024)?;
        let recipe = output::read_regular(&self.recipe, &c, 256 * 1024)?;
        if hash(&config.bytes) != self.versions.config
            || config.identity != self.versions.config_file
            || hash(&recipe.bytes) != self.versions.recipe
            || recipe.identity != self.versions.recipe_file
        {
            bail!("recovery recipe or configuration changed; preview again");
        }
        Ok(())
    }
}
struct Prepared {
    session: Session,
    input: Input,
    recipe: Recipe,
}
enum PreparedOutput {
    Preview(Plan),
    Previous(Report),
    Apply(Plan, Box<Prepared>),
}

fn prepare(
    args: &Args,
    config_path: &Path,
    state: &Path,
    profile: Option<&str>,
    c: &Control,
) -> Result<PreparedOutput> {
    let configuration = output::read_regular(config_path, c, 1024 * 1024)?;
    let recipe_file = output::read_regular(&args.recipe, c, 256 * 1024)?;
    let recipe: Recipe =
        serde_yaml::from_slice(&recipe_file.bytes).context("invalid recovery recipe")?;
    recipe.validate()?;
    let mut config = ConfigLoader::from_str_profile(
        std::str::from_utf8(&configuration.bytes)?,
        config_path,
        profile,
    )?;
    config.validate()?;
    super::resolve_working_directories(&mut config, config_path);
    let versions = Versions {
        config: hash(&configuration.bytes),
        recipe: hash(&recipe_file.bytes),
        config_file: configuration.identity,
        recipe_file: recipe_file.identity,
    };
    let input = Input {
        config: config_path.into(),
        recipe: std::path::absolute(&args.recipe)?,
        versions: versions.clone(),
    };
    let mut session = Session::open(
        config,
        config_path,
        state,
        profile,
        recipe.clone(),
        versions,
    )?;
    c.check()?;
    if let Some(id) = &args.plan {
        if let Some(report) = session.previous(id) {
            return Ok(PreparedOutput::Previous(report));
        }
    }
    let time = match &args.plan {
        Some(id) => recovery::plan_time(id)?,
        None => recovery::now_seconds()?,
    };
    let plan = session.preview(time)?;
    c.check()?;
    input.verify()?;
    if let Some(requested) = &args.plan {
        if requested != &plan.plan_id {
            bail!("recovery plan is stale; run --dry-run again");
        }
        Ok(PreparedOutput::Apply(
            plan,
            Box::new(Prepared {
                session,
                input,
                recipe,
            }),
        ))
    } else {
        Ok(PreparedOutput::Preview(plan))
    }
}

async fn worker<T: Send + 'static>(
    prepared: Arc<Mutex<Prepared>>,
    cancel: watch::Receiver<bool>,
    action: impl FnOnce(&mut Prepared) -> Result<T> + Send + 'static,
) -> Result<T> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            let mut prepared = prepared
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery worker panicked"))?;
            if *cancel.borrow() || std::time::Instant::now() >= deadline {
                bail!("recovery cancelled before accepting the next operation");
            }
            prepared.input.verify()?;
            if *cancel.borrow() || std::time::Instant::now() >= deadline {
                bail!("recovery cancelled before side effects");
            }
            prepared.session.set_operation_deadline(deadline);
            action(&mut prepared)
        }),
    )
    .await
    .context(
        "recovery file operation exceeded 30s; state lease remains held until its worker exits",
    )?
    .context("recovery worker failed")?
}

async fn apply(
    plan: Plan,
    prepared: Prepared,
    mut cancel: watch::Receiver<bool>,
) -> Result<Report> {
    let recipe = prepared.recipe.clone();
    let prepared = Arc::new(Mutex::new(prepared));
    worker(prepared.clone(), cancel.clone(), move |p| {
        p.session.begin(&plan)
    })
    .await?;
    let result = execute(prepared.clone(), &recipe, &mut cancel).await;
    // Terminal evidence is allowed after cancellation. Its worker owns the lease.
    receipt_worker(move || {
        let mut prepared = prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery worker panicked"))?;
        let (outcome, failure, safe) = match result {
            Ok(outcome) => (outcome, None, outcome != "unknown"),
            Err(error) => (
                "unknown",
                Some(format!("{error:#}; reconcile before retrying")),
                false,
            ),
        };
        prepared.session.finish(outcome, failure, safe)
    })
    .await
    .context("recovery receipt worker failed")
}

async fn execute(
    prepared: Arc<Mutex<Prepared>>,
    recipe: &Recipe,
    cancel: &mut watch::Receiver<bool>,
) -> Result<&'static str> {
    let rebuilding_cancel = cancel.clone();
    let rebuild_deadline = std::time::Instant::now() + Duration::from_secs(30);
    worker(prepared.clone(), cancel.clone(), move |p| {
        p.session.rebuild(|| {
            if *rebuilding_cancel.borrow() || std::time::Instant::now() >= rebuild_deadline {
                bail!("recovery cancelled or timed out before directory creation");
            }
            p.input.verify()?;
            if *rebuilding_cancel.borrow() || std::time::Instant::now() >= rebuild_deadline {
                bail!("recovery cancelled or timed out during input verification");
            }
            Ok(())
        })
    })
    .await?;
    for (index, step) in recipe.steps.iter().enumerate() {
        if *cancel.borrow() {
            return Ok("cancelled");
        }
        let (accepted, program, cwd) = worker(prepared.clone(), cancel.clone(), move |p| {
            let accepted = p.session.accept_step(index)?;
            Ok((accepted, p.session.program(index), p.session.root(index)))
        })
        .await?;
        if !accepted {
            return Ok("failed");
        }
        if *cancel.borrow() {
            record_step(prepared.clone(), index, "cancelled", None).await?;
            return Ok("cancelled");
        }
        worker(prepared.clone(), cancel.clone(), move |p| {
            p.session.before_spawn(index)
        })
        .await?;
        if *cancel.borrow() {
            record_step(prepared.clone(), index, "cancelled", None).await?;
            return Ok("cancelled");
        }
        let command = std::iter::once(
            program
                .to_str()
                .context("recovery program path must be UTF-8")?
                .to_owned(),
        )
        .chain(step.command[1..].iter().cloned())
        .collect::<Vec<_>>();
        let mut process = match ManagedProcess::spawn_recovery(&command, cwd).await {
            Ok(process) => process,
            Err(_) => {
                record_step(prepared.clone(), index, "failed", None).await?;
                return Ok("failed");
            }
        };
        let (state, code) = tokio::select! {
            biased;
            _ = cancel.changed() => {
                process.stop(Duration::from_secs(1)).await.context("cancelled command tree cleanup failed")?;
                ("cancelled", None)
            }
            result = tokio::time::timeout(step.duration()?, process.wait()) => match result {
                Ok(Ok(status)) => (if status.success() { "completed" } else { "failed" }, status.code()),
                Ok(Err(_)) => { process.stop(Duration::from_secs(1)).await.context("failed command cleanup failed")?; ("unknown", None) },
                Err(_) => { process.stop(Duration::from_secs(1)).await.context("timed out command tree cleanup failed")?; ("timed-out", None) },
            }
        };
        let success = record_step(prepared.clone(), index, state, code).await?;
        if !success {
            return Ok(if state == "completed" {
                "failed"
            } else {
                state
            });
        }
    }
    Ok("completed")
}

async fn record_step(
    prepared: Arc<Mutex<Prepared>>,
    index: usize,
    state: &'static str,
    code: Option<i32>,
) -> Result<bool> {
    receipt_worker(move || {
        prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery worker panicked"))?
            .session
            .complete_step(index, state, code)
    })
    .await
    .context("recovery step receipt worker failed")
}

/// Terminal evidence must survive cancellation, but never wait without a limit.
/// On timeout the closure still owns Prepared and therefore the state lease.
async fn receipt_worker<T: Send + 'static>(
    action: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(30), tokio::task::spawn_blocking(action))
        .await
        .context("recovery receipt exceeded 30s; state lease remains held until its worker exits")?
        .context("recovery receipt worker panicked")?
}

pub(super) async fn run(
    args: Args,
    config: &Path,
    state: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    let control = Control::new();
    let _guard = output::CancelOnDrop(control.clone());
    let json = args.json;
    let (config, state, profile) = (
        config.to_owned(),
        state.to_owned(),
        profile.map(str::to_owned),
    );
    let c = control.clone();
    let mut work = tokio::task::spawn_blocking(move || {
        prepare(&args, &config, &state, profile.as_deref(), &c)
    });
    let prepared = tokio::select! {
        biased;
        _ = signals.recv() => bail!("recovery preview/preparation cancelled; no commands accepted"),
        _ = tokio::time::sleep(output::DEADLINE) => bail!("recovery preparation exceeded 30s"),
        result = &mut work => result.context("recovery preparation worker failed")??,
    };
    let (value, failed) = match prepared {
        PreparedOutput::Preview(plan) => (serde_json::to_value(plan)?, false),
        PreparedOutput::Previous(report) => {
            let failed = report.outcome != "completed";
            (serde_json::to_value(report)?, failed)
        }
        PreparedOutput::Apply(plan, prepared) => {
            let (tx, rx) = watch::channel(false);
            let future = apply(plan, *prepared, rx);
            tokio::pin!(future);
            let report = tokio::select! {
                biased;
                _ = signals.recv() => { tx.send_replace(true); future.await? },
                report = &mut future => report?,
            };
            let failed = report.outcome != "completed";
            (serde_json::to_value(report)?, failed)
        }
    };
    let bytes = if json {
        output::encode(&value)?
    } else {
        // JSON quoting keeps paths and identifiers safe on a terminal; commands/arguments never appear.
        format!("Recovery {}\n{}\n{}", if value.get("expires_at").is_some() { "preview (no changes)" } else { "result" }, serde_json::to_string_pretty(&value)?, "Commands run with your permissions; no sandbox, rollback or automatic start. Verify readiness with start/wait.\n").into_bytes()
    };
    use tokio::io::AsyncWriteExt;
    let mut stdout = super::stdout::Stdout::new()?;
    tokio::select! {
        biased;
        _ = signals.recv() => bail!("recovery output cancelled; inspect retained receipts"),
        result = tokio::time::timeout(super::protocol::IO_TIMEOUT, async { stdout.write_all(&bytes).await?; stdout.write_all(b"\n").await?; stdout.flush().await }) => result.context("recovery output timed out; inspect retained receipts")??,
    }
    if failed {
        bail!("recovery did not complete; inspect receipts and reconcile effects before retrying");
    }
    Ok(())
}
