//! Root-package metadata only. Workspace declarations remain unconfirmed;
//! expanding globs or reading workspace manifests requires a separate contract.
use super::super::export::output::{self, Control};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

const MAX_PACKAGE_BYTES: usize = 1024 * 1024;
const MAX_SCRIPTS: usize = 128;
const MAX_WORKSPACES: usize = 128;

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScanReport {
    pub schema_version: u16,
    pub ecosystem: &'static str,
    pub package_json: PathBuf,
    pub package_manager: &'static str,
    pub package_manager_source: String,
    pub package_name: Option<String>,
    pub scripts: Vec<ScriptCandidate>,
    pub workspaces: Vec<String>,
    pub omitted_scripts: usize,
    pub omitted_workspaces: usize,
    pub unconfirmed: Vec<&'static str>,
    pub warnings: Vec<String>,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct ScriptCandidate {
    pub name: String,
    pub command: String,
    pub script_body: String,
    pub source: PathBuf,
    pub cwd: PathBuf,
    pub role_hint: Option<&'static str>,
}

pub(super) fn node(root: &Path, control: &Control) -> Result<ScanReport> {
    let package_json = root.join("package.json");
    let bytes = output::read_regular(&package_json, control, MAX_PACKAGE_BYTES)
        .with_context(|| {
            format!(
                "cannot read {} as a regular file (1 MiB limit)",
                package_json.display()
            )
        })?
        .bytes;
    let value: Value =
        serde_json::from_slice(&bytes).context("invalid UTF-8 JSON in package.json")?;
    let object = value
        .as_object()
        .context("package.json must contain a JSON object")?;
    let mut warnings = Vec::new();
    let (package_manager, package_manager_source) = package_manager(root, object, &mut warnings)?;
    let mut scripts = Vec::new();
    let mut omitted_scripts = 0;
    match object.get("scripts") {
        Some(Value::Object(map)) => {
            // serde_json's ordered map yields a deterministic bounded prefix.
            for (index, (name, command)) in map.iter().enumerate() {
                control.check()?;
                if index >= MAX_SCRIPTS {
                    omitted_scripts += 1;
                    continue;
                }
                let Some(command) = command
                    .as_str()
                    .filter(|command| !command.trim().is_empty())
                else {
                    omitted_scripts += 1;
                    warnings.push(format!("ignored non-string or empty script {name:?}"));
                    continue;
                };
                if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_control) {
                    omitted_scripts += 1;
                    warnings.push(format!("ignored unsupported script name {name:?}"));
                    continue;
                }
                scripts.push(ScriptCandidate {
                    name: name.clone(),
                    command: shell_words::join([
                        package_manager_executable(package_manager),
                        "run",
                        name.as_str(),
                    ]),
                    script_body: command.to_owned(),
                    source: package_json.clone(),
                    cwd: root.to_owned(),
                    role_hint: role_hint(name),
                });
            }
        }
        None => warnings.push("package.json has no scripts object".into()),
        Some(_) => bail!("package.json scripts must be an object"),
    }
    let (workspaces, omitted_workspaces) = workspaces(object, &mut warnings);
    if !workspaces.is_empty() {
        warnings.push("workspace declarations are unconfirmed: patterns are not expanded and workspace manifests are not scanned".into());
    }
    control.check()?;
    Ok(ScanReport {
        schema_version: 1,
        ecosystem: "node",
        package_json,
        package_manager,
        package_manager_source,
        package_name: object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        scripts,
        workspaces,
        omitted_scripts,
        omitted_workspaces,
        unconfirmed: vec![
            "script role and lifecycle",
            "task success semantics",
            "workspace relationships",
            "health checks",
            "ports",
            "environment files",
        ],
        complete: warnings.is_empty() && omitted_scripts == 0 && omitted_workspaces == 0,
        warnings,
    })
}

fn package_manager_executable(manager: &'static str) -> &'static str {
    if cfg!(windows) {
        match manager {
            "npm" => "npm.cmd",
            "pnpm" => "pnpm.cmd",
            "yarn" => "yarn.cmd",
            _ => manager,
        }
    } else {
        manager
    }
}

fn package_manager(
    root: &Path,
    package: &Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<(&'static str, String)> {
    let managers = [
        ("npm", "package-lock.json"),
        ("pnpm", "pnpm-lock.yaml"),
        ("yarn", "yarn.lock"),
        ("bun", "bun.lock"),
        ("bun", "bun.lockb"),
    ];
    let mut locks = BTreeSet::new();
    for (manager, filename) in managers {
        match std::fs::symlink_metadata(root.join(filename)) {
            Ok(metadata) if crate::platform::files::regular(&metadata) => {
                locks.insert(manager);
            }
            Ok(_) => warnings.push(format!(
                "ignored non-regular package-manager hint {filename}"
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect package-manager hints"),
        }
    }
    if let Some(declared) = package.get("packageManager") {
        let declared = declared
            .as_str()
            .context("packageManager must be a string")?;
        if let Some((manager, _)) = managers
            .iter()
            .find(|(manager, _)| declared.starts_with(&format!("{manager}@")))
        {
            if locks.iter().any(|lock| lock != manager) {
                warnings.push("packageManager conflicts with lockfile hints; using the explicit packageManager field".into());
            }
            return Ok((manager, "package.json#packageManager".into()));
        }
        bail!("unsupported packageManager {declared:?}; choose npm, pnpm, yarn or bun explicitly in package.json");
    }
    if locks.len() > 1 {
        bail!("conflicting package-manager lockfile hints; set packageManager explicitly in package.json");
    }
    Ok(match locks.first() {
        Some(manager) => (manager, "lockfile metadata".into()),
        None => {
            warnings.push("no package-manager hint; npm is an unconfirmed default".into());
            ("npm", "default (unconfirmed)".into())
        }
    })
}

fn role_hint(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let prefix = name.split(':').next().unwrap_or_default();
    match prefix {
        "build" | "compile" | "generate" | "codegen" | "migrate" => Some("task"),
        "dev" | "start" | "serve" | "watch" => Some("consumer"),
        _ => None,
    }
}

fn workspaces(package: &Map<String, Value>, warnings: &mut Vec<String>) -> (Vec<String>, usize) {
    let entries = match package.get("workspaces") {
        None => return (Vec::new(), 0),
        Some(Value::Array(entries)) => Some(entries),
        Some(Value::Object(object)) => object.get("packages").and_then(Value::as_array),
        Some(_) => None,
    };
    let Some(entries) = entries else {
        warnings
            .push("unsupported workspaces declaration; expected an array or packages array".into());
        return (Vec::new(), 1);
    };
    let mut omitted = entries.len().saturating_sub(MAX_WORKSPACES);
    let mut result = BTreeSet::new();
    for value in entries.iter().take(MAX_WORKSPACES) {
        match value.as_str().filter(|entry| !entry.is_empty()) {
            Some(entry) => {
                result.insert(entry.to_owned());
            }
            None => {
                omitted += 1;
                warnings.push("ignored non-string or empty workspace entry".into());
            }
        }
    }
    (result.into_iter().collect(), omitted)
}
