use std::{
    collections::BTreeMap,
    io::{self, BufRead, Read, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde_json::Value;

use crate::{
    config::ConfigLoader,
    core::service_manager::{ManagerOptions, ServiceManager},
};

mod scan;
#[cfg(test)]
mod tests;

use super::export::output::{CancelOnDrop, Control, DEADLINE};
use scan::ScanReport;

const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: u64 = 8192;
const GENERATED_TASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum Ecosystem {
    Node,
}

#[derive(clap::Args)]
pub(super) struct Args {
    /// Name of the first service in the starter configuration.
    #[arg(long, default_value = "app")]
    service: String,
    /// Command for the legacy one-service starter.
    #[arg(long, conflicts_with = "scan")]
    command: Option<String>,
    /// Scan a supported project manifest and propose a configuration.
    #[arg(long, value_enum)]
    scan: Option<Ecosystem>,
    /// Select a package script as the successful one-shot task.
    #[arg(long)]
    build_script: Option<String>,
    /// Select a package script as the long-running consumer.
    #[arg(long)]
    consumer_script: Option<String>,
    /// Set the explicit dependency readiness timeout for a scanned task stack.
    #[arg(long, value_parser = crate::config::parse_duration)]
    dependency_timeout: Option<std::time::Duration>,
    /// Read script selections and confirmation from stdin.
    #[arg(long, conflicts_with_all = ["yes", "json"])]
    interactive: bool,
    /// Publish the reviewed scanned configuration.
    #[arg(long)]
    yes: bool,
    /// Print the scan or generated configuration as JSON.
    #[arg(long)]
    json: bool,
}

pub(super) async fn run(args: Args, config_path: &Path, options: ManagerOptions) -> Result<()> {
    let mut signals = crate::platform::shutdown::Shutdown::new(false)?;
    if let Some(ecosystem) = args.scan {
        return run_scan(args, ecosystem, config_path, options, &mut signals).await;
    }
    if args.build_script.is_some()
        || args.consumer_script.is_some()
        || args.dependency_timeout.is_some()
        || args.interactive
        || args.yes
        || args.json
    {
        bail!("--build-script, --consumer-script, --dependency-timeout, --interactive, --yes and --json require --scan node");
    }
    let command = args.command.unwrap_or_else(default_command);
    let yaml = serde_yaml::to_string(&serde_json::json!({
        "version": "1",
        "services": { (args.service): { "command": command } },
    }))?;
    validate_yaml(&yaml, config_path, options)?;
    publish(config_path, yaml, &mut signals).await?;
    write_output(
        &format!("Created {}\n", config_path.display()),
        &mut signals,
    )
    .await
}

async fn run_scan(
    args: Args,
    ecosystem: Ecosystem,
    config_path: &Path,
    options: ManagerOptions,
    signals: &mut crate::platform::shutdown::Shutdown,
) -> Result<()> {
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let root = config_path
        .parent()
        .context("configuration path has no project directory")?
        .to_owned();
    // One read-only worker per init invocation. Cancelling cannot write files or
    // execute programs; OS calls may finish after cancellation, but publish is separate.
    let work = tokio::task::spawn_blocking(move || match ecosystem {
        Ecosystem::Node => scan::node(&root, &control),
    });
    let report = tokio::select! {
        biased;
        _ = signals.recv() => bail!("project scan cancelled"),
        result = tokio::time::timeout(DEADLINE, work) => result.context("project scan exceeded 30s")?.context("project scan worker failed")??,
    };
    if args.build_script.is_none() && args.consumer_script.is_none() && !args.interactive {
        if args.yes {
            bail!("--yes requires --build-script and --consumer-script");
        }
        return write_output(&render_scan(&report, args.json)?, signals).await;
    }
    let build = if args.interactive {
        write_output(&render_scan(&report, false)?, signals).await?;
        select_script("Build/task script", &report, args.build_script, signals).await?
    } else {
        args.build_script.context("--build-script is required")?
    };
    let consumer = if args.interactive {
        select_script(
            "Consumer/service script",
            &report,
            args.consumer_script,
            signals,
        )
        .await?
    } else {
        args.consumer_script
            .context("--consumer-script is required")?
    };
    let dependency_timeout = args
        .dependency_timeout
        .unwrap_or_else(|| std::time::Duration::from_secs(600));
    let yaml = generate_yaml(
        &report,
        &build,
        &consumer,
        &args.service,
        dependency_timeout,
    )?;
    validate_yaml(&yaml, config_path, options)?;
    let next_steps = next_steps(config_path, dependency_timeout);
    let review = format!("{}\nReview command, cwd, task completion semantics and timeouts:\n{}\nUnselected scripts and workspace relationships are not enabled.\nAfter publication (start stays in the foreground; wait runs in another terminal):\n{}\n", render_scan(&report, false)?, yaml, next_steps.join("\n"));
    let confirmed = if args.interactive {
        write_output(&review, signals).await?;
        prompt("Write this configuration? [y/N] ", signals)
            .await?
            .eq_ignore_ascii_case("y")
    } else {
        args.yes
    };
    let output = if args.json {
        serde_json::to_string_pretty(&serde_json::json!({
            "scan": report,
            "configuration": serde_yaml::from_str::<Value>(&yaml)?,
            "published": confirmed,
            "next_steps": next_steps,
        }))? + "\n"
    } else if confirmed {
        format!("{review}\nCreated {}\n", config_path.display())
    } else {
        format!(
            "{review}\nNo file written. Re-run with --yes to publish {}.\n",
            config_path.display()
        )
    };
    // Validate the full response before accepting publication; emit success only
    // after the no-clobber rename succeeds.
    check_output(&output)?;
    if confirmed {
        publish(config_path, yaml, signals).await?;
    }
    write_output(&output, signals).await
}

fn next_steps(config_path: &Path, dependency_timeout: std::time::Duration) -> Vec<String> {
    ["check", "graph", "doctor", "start", "wait"]
        .into_iter()
        .map(|command| {
            let path = config_path.to_string_lossy();
            let mut arguments = vec![
                "devd".to_owned(),
                "--config".to_owned(),
                path.into_owned(),
                command.to_owned(),
            ];
            if command == "wait" {
                arguments.extend([
                    "--timeout".into(),
                    format_duration(dependency_timeout.min(std::time::Duration::from_secs(3600))),
                ]);
            }
            shell_words::join(arguments)
        })
        .collect()
}

fn validate_yaml(yaml: &str, config_path: &Path, options: ManagerOptions) -> Result<()> {
    ServiceManager::new(ConfigLoader::from_str(yaml, config_path)?, options)
        .context("generated configuration failed validation")?;
    Ok(())
}

async fn publish(
    path: &Path,
    yaml: String,
    signals: &mut crate::platform::shutdown::Shutdown,
) -> Result<()> {
    let path = path.to_owned();
    let control = Control::new();
    let _cancel = CancelOnDrop(control.clone());
    let work = tokio::task::spawn_blocking(move || publish_blocking(&path, &yaml, &control));
    tokio::select! {
        biased;
        _ = signals.recv() => bail!("init cancelled; an accepted publication may have completed"),
        result = tokio::time::timeout(DEADLINE, work) => result.context("init publication exceeded 30s; an accepted publication may have completed")?.context("init publication worker failed")?,
    }
}

fn publish_blocking(path: &Path, yaml: &str, control: &Control) -> Result<()> {
    control.check()?;
    #[cfg(windows)]
    super::snapshot::validate_windows_filename(path)?;
    let directory = path.parent().context("configuration path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    control.check()?;
    temporary.write_all(yaml.as_bytes())?;
    temporary.as_file().sync_all()?;
    control.publish()?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::Error::new(error.error).context(format!(
            "cannot create {}; an existing configuration will not be replaced",
            path.display()
        ))
    })?;
    Ok(())
}

fn render_scan(report: &ScanReport, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(report)? + "\n");
    }
    let mut text = format!(
        "Node project: {:?}\nPackage manager: {} (hint only)\n",
        report.package_json, report.package_manager
    );
    text.push_str(&format!(
        "Workspace declarations (unconfirmed): {:?}\nOmitted scripts: {}  Omitted workspaces: {}\n",
        report.workspaces, report.omitted_scripts, report.omitted_workspaces
    ));
    for candidate in &report.scripts {
        text.push_str(&format!(
            "  {:?}: {:?} script_body={:?} role_hint={:?} cwd={:?}\n",
            candidate.name,
            candidate.command,
            candidate.script_body,
            candidate.role_hint,
            candidate.cwd
        ));
    }
    for warning in &report.warnings {
        text.push_str(&format!("Warning: {warning:?}\n"));
    }
    text.push_str("Select --build-script NAME and --consumer-script NAME, or use --interactive.\n");
    Ok(text)
}

async fn select_script(
    label: &str,
    report: &ScanReport,
    selected: Option<String>,
    signals: &mut crate::platform::shutdown::Shutdown,
) -> Result<String> {
    let name = match selected {
        Some(name) => name,
        None => prompt(&format!("{label} (empty to cancel): "), signals).await?,
    };
    if !report
        .scripts
        .iter()
        .any(|candidate| candidate.name == name)
    {
        bail!("script {:?} is not a supported candidate", name);
    }
    Ok(name)
}

async fn prompt(text: &str, signals: &mut crate::platform::shutdown::Shutdown) -> Result<String> {
    write_output(text, signals).await?;
    // A bounded, read-only stdin thread does not hold Tokio's blocking pool open
    // after Ctrl+C. At most one prompt is pending; it owns no publication right.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("devd-init-input".into())
        .spawn(move || {
            let result = (|| -> Result<String> {
                let mut bytes = Vec::new();
                io::stdin()
                    .lock()
                    .take(MAX_INPUT_BYTES + 1)
                    .read_until(b'\n', &mut bytes)?;
                if bytes.len() as u64 > MAX_INPUT_BYTES {
                    bail!("init selection exceeds 8 KiB");
                }
                Ok(String::from_utf8(bytes)?
                    .trim_end_matches(['\r', '\n'])
                    .to_owned())
            })();
            let _ = sender.send(result);
        })?;
    tokio::select! {
        biased;
        _ = signals.recv() => bail!("init selection cancelled"),
        result = receiver => result.context("init input thread failed")?,
    }
}

async fn write_output(text: &str, signals: &mut crate::platform::shutdown::Shutdown) -> Result<()> {
    let mut stdout = super::stdout::Stdout::new()?;
    write_to(&mut stdout, text, signals.recv()).await
}

fn check_output(text: &str) -> Result<()> {
    if text.len() > MAX_OUTPUT_BYTES {
        bail!("init output exceeds 2 MiB");
    }
    Ok(())
}

async fn write_to(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    text: &str,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    check_output(text)?;
    tokio::select! {
        biased;
        _ = cancelled => bail!("init output cancelled; output may be incomplete"),
        result = tokio::time::timeout(super::protocol::IO_TIMEOUT, async {
            writer.write_all(text.as_bytes()).await?;
            writer.flush().await
        }) => result.context("init output timed out after 5s")?.context("cannot write init output"),
    }
}

fn generate_yaml(
    report: &ScanReport,
    build_script: &str,
    consumer_script: &str,
    consumer_name: &str,
    dependency_timeout: std::time::Duration,
) -> Result<String> {
    if dependency_timeout < GENERATED_TASK_TIMEOUT {
        bail!(
            "--dependency-timeout must be at least {} when the generated task-timeout is 5m",
            format_duration(GENERATED_TASK_TIMEOUT)
        );
    }
    let build = report
        .scripts
        .iter()
        .find(|candidate| candidate.name == build_script)
        .context("build script was not found")?;
    let consumer = report
        .scripts
        .iter()
        .find(|candidate| candidate.name == consumer_script)
        .context("consumer script was not found")?;
    let task_name = format!("{}-task", sanitize_name(build_script));
    let consumer_name = consumer_name.to_owned();
    if task_name == consumer_name {
        bail!(
            "generated task service '{task_name}' conflicts with the consumer service name; choose a different --service"
        );
    }
    let mut task = BTreeMap::new();
    task.insert("kind", Value::String("task".into()));
    task.insert("cwd", Value::String(".".into()));
    task.insert("command", Value::String(build.command.clone()));
    task.insert(
        "task-timeout",
        Value::String(format_duration(GENERATED_TASK_TIMEOUT)),
    );
    let dependency = serde_json::json!({
        "service": task_name,
        "condition": "completed-successfully",
    });
    let consumer_value = serde_json::json!({
        "command": consumer.command,
        "cwd": ".",
        "depends-on": [dependency],
        "restart": {"policy": "never"},
    });
    let root = serde_json::json!({
        "version": "1",
        "dependency-timeout": format_duration(dependency_timeout),
        "services": {
            (task_name): task,
            (consumer_name): consumer_value,
        },
    });
    Ok(serde_yaml::to_string(&root)?)
}

fn sanitize_name(value: &str) -> String {
    let mut name: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        name.insert(0, '_');
    }
    name
}

fn format_duration(duration: std::time::Duration) -> String {
    if duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn default_command() -> String {
    #[cfg(unix)]
    {
        "sh -c 'echo app-ready; exec sleep 3600'".into()
    }
    #[cfg(windows)]
    {
        "powershell.exe -NoLogo -NoProfile -Command 'Write-Output app-ready; Start-Sleep -Seconds 3600'".into()
    }
}
