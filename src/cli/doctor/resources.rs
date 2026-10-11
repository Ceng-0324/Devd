use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    config::{DevdConfig, PathRequirementType, PathScope},
    core::{
        path_requirements,
        resource_inspection::{self, Inspection},
    },
};

use super::{check, path_requirement_failure, required_check_name, CheckStatus, DoctorCheck};

const MAX_RESOURCES: usize = 128;
const MAX_REPORT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "field", rename_all = "kebab-case")]
enum Declaration {
    Requires {
        index: usize,
        requirement: PathRequirementType,
        condition: Condition,
    },
    Paths {
        name: String,
        scope: PathScope,
    },
}

#[derive(Debug, Serialize)]
struct Resource {
    service: String,
    /// Refers to the selected effective configuration, not a YAML line number.
    declaration: Declaration,
    declared_path: String,
    cwd: String,
    requested_path: Option<String>,
    display_paths_lossy: bool,
    inspection: Inspection,
}

#[derive(Debug, Serialize)]
struct Condition {
    satisfied: bool,
    failure: Option<path_requirements::PathRequirementFailureKind>,
}

#[derive(Debug, Serialize)]
pub(super) struct ResourceReport {
    schema_version: u16,
    source: &'static str,
    state_dir: String,
    captured_from: DateTime<Utc>,
    captured_to: DateTime<Utc>,
    complete: bool,
    omitted: usize,
    limits: Limits,
    entries: Vec<Resource>,
}

#[derive(Debug, Serialize)]
struct Limits {
    resources: usize,
    nodes_per_resource: usize,
    links_per_resource: usize,
    path_bytes: usize,
    chain_json_bytes: usize,
    entries_json_bytes: usize,
}

pub(super) fn collect(config: &DevdConfig, state_dir: &Path) -> (Vec<DoctorCheck>, ResourceReport) {
    let mut report = ResourceReport {
        schema_version: 1,
        source: "effective-config",
        state_dir: state_dir.to_string_lossy().into_owned(),
        captured_from: Utc::now(),
        captured_to: Utc::now(),
        complete: true,
        omitted: 0,
        limits: Limits {
            resources: MAX_RESOURCES,
            nodes_per_resource: resource_inspection::MAX_NODES,
            links_per_resource: resource_inspection::MAX_LINKS,
            path_bytes: resource_inspection::MAX_PATH_BYTES,
            chain_json_bytes: resource_inspection::MAX_CHAIN_BYTES,
            entries_json_bytes: MAX_REPORT_BYTES,
        },
        entries: vec![],
    };
    let mut checks = vec![];
    let mut bytes = 2;
    let mut names: Vec<_> = config.services.keys().collect();
    names.sort();
    for name in names {
        let service = &config.services[name];
        let cwd = service.cwd.as_deref().unwrap_or(Path::new("."));
        for (index, requirement) in service.requires.iter().enumerate() {
            let path = path_requirements::resolve_path(&requirement.path, Some(cwd));
            // The existing evaluator remains the sole authority on access and
            // startup eligibility, independent of inspection limits/uncertainty.
            let evaluated = path_requirements::evaluate(requirement, Some(cwd));
            let condition = Condition {
                satisfied: evaluated.is_ok(),
                failure: evaluated.as_ref().err().map(|error| error.kind),
            };
            checks.push(match evaluated {
                Ok(path) => check(
                    Some(name),
                    required_check_name(requirement.kind),
                    CheckStatus::Passed,
                    "Required path exists, has the declared type, and is readable.",
                    vec![path.display().to_string()],
                    None,
                ),
                Err(error) => path_requirement_failure(name, error),
            });
            append(
                &mut report,
                &mut bytes,
                name,
                cwd,
                &requirement.path,
                Declaration::Requires {
                    index,
                    requirement: requirement.kind,
                    condition,
                },
                Ok(path),
            );
        }
        for (key, binding) in &service.paths {
            append(
                &mut report,
                &mut bytes,
                name,
                cwd,
                &binding.path,
                Declaration::Paths {
                    name: key.clone(),
                    scope: binding.scope,
                },
                binding.resolve(Some(cwd), state_dir),
            );
        }
    }
    report.captured_to = Utc::now();
    (checks, report)
}

fn append(
    report: &mut ResourceReport,
    bytes: &mut usize,
    service: &str,
    cwd: &Path,
    declared: &Path,
    declaration: Declaration,
    path: std::io::Result<PathBuf>,
) {
    // Stop scheduling filesystem work once output capacity has been exhausted.
    if report.entries.len() == MAX_RESOURCES || report.omitted > 0 {
        report.omitted += 1;
        report.complete = false;
        return;
    }
    let requested_path = path.as_ref().ok().map(|p| p.to_string_lossy().into_owned());
    let display_paths_lossy = declared.to_str().is_none()
        || cwd.to_str().is_none()
        || path.as_ref().is_ok_and(|p| p.to_str().is_none());
    let inspection = match path {
        Ok(path) => resource_inspection::inspect(&path),
        Err(_) => Inspection {
            nodes: vec![],
            resolved: None,
            failure: Some(resource_inspection::Failure::Inspection),
            failed_node: None,
            atomic: false,
            change_detected: false,
            complete: false,
        },
    };
    let resource = Resource {
        service: service.into(),
        declaration,
        declared_path: declared.to_string_lossy().into_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        requested_path,
        display_paths_lossy,
        inspection,
    };
    let size = serde_json::to_vec(&resource)
        .expect("string-only resource serialization")
        .len()
        + 1;
    if *bytes + size > MAX_REPORT_BYTES {
        report.omitted += 1;
        report.complete = false;
        return;
    }
    *bytes += size;
    report.complete &= resource.inspection.complete;
    report.entries.push(resource);
}

pub(super) fn render(report: &ResourceReport, text: &mut String) {
    use super::safe_text;
    text.push_str(
        "Resource resolution (effective configuration; non-atomic metadata observations):\n",
    );
    text.push_str(&format!(
        "  State directory: {}\n",
        safe_text(&report.state_dir)
    ));
    for resource in &report.entries {
        let declaration = match &resource.declaration {
            Declaration::Requires {
                index, requirement, ..
            } => {
                format!("requires[{index}] ({requirement:?})")
            }
            Declaration::Paths { name, scope } => format!("paths.{name} ({scope:?})"),
        };
        text.push_str(&format!(
            "  [{}] {}: {}\n    cwd: {}\n",
            safe_text(&resource.service),
            safe_text(&declaration),
            safe_text(&resource.declared_path),
            safe_text(&resource.cwd)
        ));
        if let Some(path) = &resource.requested_path {
            text.push_str(&format!("    requested: {}\n", safe_text(path)));
        }
        if resource.display_paths_lossy {
            text.push_str(
                "    warning: displayed paths contain substitutions; exact identity unavailable\n",
            );
        }
        for (index, node) in resource.inspection.nodes.iter().enumerate() {
            text.push_str(&format!(
                "    {index}: {:?} {}",
                node.kind,
                safe_text(&node.path)
            ));
            if let Some(target) = &node.target {
                text.push_str(&format!(" -> {}", safe_text(target)));
            }
            text.push('\n');
        }
        text.push_str(&format!(
            "    result: {}; failed node: {}; change detected: {}; complete: {}\n",
            resource
                .inspection
                .failure
                .map(|f| format!("{f:?}"))
                .unwrap_or_else(|| "resolved".into()),
            resource
                .inspection
                .failed_node
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown/none".into()),
            resource.inspection.change_detected,
            resource.inspection.complete
        ));
    }
    text.push_str(&format!(
        "Resource coverage: complete={}, omitted={}\n",
        report.complete, report.omitted
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_entry_json_budget_stops_before_count_limit() {
        let root = tempfile::tempdir().unwrap();
        let mut service: crate::config::ServiceConfig =
            serde_yaml::from_str("command: unused").unwrap();
        service.cwd = Some(root.path().to_owned());
        // Control bytes expand in JSON, so raw byte accounting is insufficient.
        for index in 0..100 {
            service.paths.insert(
                format!("PATH_{index:03}"),
                crate::config::RuntimePath {
                    scope: PathScope::Shared,
                    path: format!("{}-{index}", "\u{1b}".repeat(1000)).into(),
                    cleanup: false,
                },
            );
        }
        let config = DevdConfig {
            version: "1".into(),
            services: [("api".into(), service)].into(),
        };
        let (checks, report) = collect(&config, root.path());
        assert!(checks.is_empty());
        assert!(report.omitted > 0);
        assert!(report.entries.len() < MAX_RESOURCES);
        assert_eq!(report.entries.len() + report.omitted, 100);
        assert!(!report.complete);
        assert!(serde_json::to_vec(&report.entries).unwrap().len() <= MAX_REPORT_BYTES);
    }
}
