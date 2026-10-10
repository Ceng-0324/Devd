//! Configuration impact planning and execution reports. No process access.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    dependency::DependencyGraph,
    service_manager::{prepare_config, RuntimeSnapshot, ServiceState},
};
use crate::config::{DevdConfig, HealthCheck, PathRequirementType, PathScope};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    DependencyAffected,
    Unchanged,
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Modified => "modified",
            Self::DependencyAffected => "dependency-affected",
            Self::Unchanged => "unchanged",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceImpact {
    pub change: ChangeKind,
    /// Top-level field names only; values can contain credentials.
    pub changed_fields: Vec<String>,
    /// Directly changed prerequisites reachable through either dependency graph.
    pub affected_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub generation: Option<u64>,
    pub pid: Option<u32>,
    pub status: ServiceState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadPlan {
    pub schema_version: u16,
    pub run_id: String,
    pub profile: Option<String>,
    pub base_config_id: String,
    pub candidate_config_id: String,
    pub plan_id: String,
    pub runtime: BTreeMap<String, ProcessIdentity>,
    pub services: BTreeMap<String, ServiceImpact>,
    pub stop_layers: Vec<Vec<String>>,
    pub start_layers: Vec<Vec<String>>,
    /// Application still requires recomputation and matching plan identity.
    pub apply_available: bool,
    pub unsupported_changes: Vec<String>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReloadOutcome {
    Applied,
    Failed,
    Interrupted,
}

/// Progress is factual: stopped actors have finished; started services acquired
/// a PID. Neither list implies rollback or that a process is still alive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadReport {
    pub schema_version: u16,
    pub plan_id: String,
    pub run_id: String,
    pub base_config_id: String,
    pub candidate_config_id: String,
    pub outcome: ReloadOutcome,
    pub config_committed: bool,
    pub stopped: Vec<String>,
    pub started: Vec<String>,
    pub ready: Vec<String>,
    pub failure: Option<String>,
}

/// Validate the entire candidate, compare effective definitions, and compute a
/// conservative transitive impact through the union of old and new edges.
pub fn preview(
    base: &DevdConfig,
    candidate: &DevdConfig,
    snapshot: &RuntimeSnapshot,
    profile: Option<&str>,
) -> Result<ReloadPlan> {
    let old_layers = prepare_config(base, Path::new("."))?.layers;
    let new_layers = prepare_config(candidate, Path::new("."))?.layers;
    let old_graph = DependencyGraph::from_config(base)?;
    let new_graph = DependencyGraph::from_config(candidate)?;
    let old = normalized(base)?;
    let new = normalized(candidate)?;
    let names: BTreeSet<_> = base
        .services
        .keys()
        .chain(candidate.services.keys())
        .cloned()
        .collect();
    let mut services = BTreeMap::new();
    let mut changed = BTreeSet::new();
    for name in names {
        let before = old["services"].get(&name);
        let after = new["services"].get(&name);
        let kind = match (before, after) {
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Removed,
            (Some(a), Some(b)) if a != b => ChangeKind::Modified,
            _ => ChangeKind::Unchanged,
        };
        let changed_fields = match (before, after) {
            (Some(a), Some(b)) => a
                .as_object()
                .context("service must be an object")?
                .keys()
                .filter(|field| a[*field] != b[*field])
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        if kind != ChangeKind::Unchanged {
            changed.insert(name.clone());
        }
        services.insert(
            name,
            ServiceImpact {
                change: kind,
                changed_fields,
                affected_by: Vec::new(),
            },
        );
    }
    for root in &changed {
        let mut visited = BTreeSet::from([root.clone()]);
        let mut queue = VecDeque::from([root.clone()]);
        while let Some(name) = queue.pop_front() {
            for child in old_graph
                .dependents(&name)
                .unwrap_or_default()
                .iter()
                .chain(new_graph.dependents(&name).unwrap_or_default())
            {
                if !visited.insert(child.clone()) {
                    continue;
                }
                queue.push_back(child.clone());
                let impact = services
                    .get_mut(child)
                    .expect("graph service was collected");
                impact.affected_by.push(root.clone());
                if impact.change == ChangeKind::Unchanged {
                    impact.change = ChangeKind::DependencyAffected;
                }
            }
        }
    }
    let retain_affected = |layers: Vec<Vec<String>>| -> Vec<Vec<String>> {
        layers
            .into_iter()
            .map(|layer| {
                layer
                    .into_iter()
                    .filter(|name| services[name].change != ChangeKind::Unchanged)
                    .collect::<Vec<_>>()
            })
            .filter(|layer| !layer.is_empty())
            .collect()
    };
    let mut stop_layers = retain_affected(old_layers);
    stop_layers.reverse();
    let start_layers = retain_affected(new_layers);
    let runtime = snapshot
        .services
        .iter()
        .map(|(name, state)| {
            (
                name.clone(),
                ProcessIdentity {
                    generation: state.event_generation,
                    pid: state.pid,
                    status: state.status,
                },
            )
        })
        .collect();
    let mut plan = ReloadPlan {
        schema_version: 1,
        run_id: snapshot.event_run_id.clone().context("supervisor has no run identity")?,
        profile: profile.map(str::to_owned),
        base_config_id: fingerprint(&old)?, candidate_config_id: fingerprint(&new)?,
        plan_id: String::new(), runtime, services, stop_layers, start_layers,
        apply_available: true,
        unsupported_changes: Vec::new(),
        limitations: vec![
            "Instance/profile, state directory and supervisor logging options are fixed; this command cannot change them.".into(),
            "Only YAML definitions are compared. Env-file contents, inherited environment, program contents and current path/port readiness are not inspected.".into(),
            "Orders conservatively include all downstream services in both graphs, independently of restart-on-dep-recovery. Preview changes no process.".into(),
            "Apply requires --apply --plan <plan_id> and a fresh matching candidate/runtime. Re-run preview after any input or lifecycle change.".into(),
            "Reload failure stops the whole stack. There is no automatic rollback; affected services do not automatically recover during application.".into(),
        ],
    };
    plan.plan_id = fingerprint(&serde_json::to_value(&plan)?)?;
    Ok(plan)
}

fn fingerprint(value: &serde_json::Value) -> Result<String> {
    let mut value = value.clone();
    value.sort_all_objects();
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&value)?)
    ))
}

fn normalized(config: &DevdConfig) -> Result<serde_json::Value> {
    let mut config = config.clone();
    for service in config.services.values_mut() {
        service.command = shell_words::join(shell_words::split(&service.command)?);
        service.depends_on.sort_by(|a, b| a.service.cmp(&b.service));
        service.listen.sort();
        service.listen.dedup();
        let cwd = service.cwd.as_deref().unwrap_or(Path::new("."));
        for binding in service.paths.values_mut() {
            if binding.scope == PathScope::Shared {
                binding.path = clean_path(&cwd.join(&binding.path), false);
            }
        }
        for requirement in &mut service.requires {
            requirement.path = clean_path(
                &cwd.join(&requirement.path),
                requirement.kind == PathRequirementType::Directory,
            );
        }
        if let Some(path) = &mut service.env_file {
            *path = clean_path(&cwd.join(&*path), false);
        }
        match &mut service.healthcheck {
            Some(HealthCheck::Socket { path, .. }) => {
                *path = clean_path(&cwd.join(&*path), false);
            }
            Some(HealthCheck::Script { command, .. }) => {
                *command = shell_words::join(shell_words::split(command)?);
            }
            _ => {}
        }
        service.cwd = Some(clean_path(cwd, true));
        if let Some(limits) = &mut service.limits {
            if let Some(thresholds) = limits.thresholds() {
                limits.cpu = thresholds.cpu_percent.map(|v| format!("{v}%"));
                limits.memory = thresholds.memory_bytes.map(|v| format!("{v}B"));
            }
        }
    }
    Ok(serde_json::to_value(config)?)
}

fn clean_path(path: &Path, requires_directory: bool) -> PathBuf {
    // Do not collapse '..': that changes meaning in the presence of symlinks.
    let mut clean: PathBuf = path
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    if clean.as_os_str().is_empty() {
        clean.push(".");
    }
    // A trailing separator or '/.' requires a directory on Unix. In particular,
    // 'file/.' must not compare equal to the readable regular file 'file'.
    let raw = path.as_os_str().to_string_lossy();
    if !requires_directory
        && clean.file_name().is_some()
        && (raw.ends_with(std::path::is_separator)
            || raw.ends_with("/.")
            || (cfg!(windows) && raw.ends_with("\\.")))
    {
        clean.push(".");
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ConfigLoader, core::service_manager::ServiceSnapshot};

    fn config(services: &str) -> DevdConfig {
        ConfigLoader::from_str(&format!("version: '1'\nservices:\n{services}"), "devd.yml").unwrap()
    }

    fn snapshot(config: &DevdConfig) -> RuntimeSnapshot {
        RuntimeSnapshot {
            supervisor_pid: 42,
            event_run_id: Some("run-a".into()),
            services: config
                .services
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        ServiceSnapshot {
                            status: ServiceState::Running,
                            pid: Some(100),
                            event_generation: Some(1),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn test_reload_preview_add_remove_modify_and_transitive_impact() {
        let base = config("  db: {command: db}\n  api: {command: api, depends-on: [db]}\n  web: {command: web, depends-on: [api]}\n  isolated: {command: isolated}\n  removed: {command: removed}\n");
        let candidate = config("  db: {command: db-new}\n  api: {command: api, depends-on: [db]}\n  web: {command: web, depends-on: [api]}\n  isolated: {command: isolated}\n  added: {command: added, depends-on: [db]}\n");
        let plan = preview(&base, &candidate, &snapshot(&base), None).unwrap();
        assert_eq!(plan.services["db"].change, ChangeKind::Modified);
        assert_eq!(plan.services["db"].changed_fields, ["command"]);
        for name in ["api", "web"] {
            assert_eq!(plan.services[name].change, ChangeKind::DependencyAffected);
            assert_eq!(plan.services[name].affected_by, ["db"]);
        }
        assert_eq!(plan.services["added"].change, ChangeKind::Added);
        assert_eq!(plan.services["removed"].change, ChangeKind::Removed);
        assert_eq!(plan.services["isolated"].change, ChangeKind::Unchanged);
        assert_eq!(
            plan.stop_layers,
            [vec!["web"], vec!["api"], vec!["db", "removed"]]
        );
        assert_eq!(
            plan.start_layers,
            [vec!["db"], vec!["added", "api"], vec!["web"]]
        );
        assert!(plan.apply_available);
        assert!(plan.unsupported_changes.is_empty());
    }

    #[test]
    fn test_reload_preview_normalizes_equivalent_effective_definitions() {
        let base = config("  a: {command: worker, limits: {memory: 1024KiB}}\n  b: {command: worker}\n  api:\n    command: 'worker  abc'\n    cwd: /project/./api\n    env: {Z: secret, A: value}\n    env-file: ./input.env\n    requires: [{type: file, path: ./data}]\n    depends-on: [b, a]\n    restart: {initial-delay: 1s}\n");
        let candidate = config("  api:\n    command: \"worker 'abc'\"\n    cwd: /project/api\n    env: {A: value, Z: secret}\n    env_file: /project/api/input.env\n    requires: [{type: file, path: /project/api/data}]\n    depends_on: [{service: a, condition: started}, b]\n    restart: {initial_delay: 1000ms}\n  b: {command: worker}\n  a: {command: worker, limits: {memory: 1MiB}}\n");
        let plan = preview(&base, &candidate, &snapshot(&base), Some("dev")).unwrap();
        assert!(plan
            .services
            .values()
            .all(|impact| impact.change == ChangeKind::Unchanged));
        assert_eq!(plan.base_config_id, plan.candidate_config_id);
        assert!(plan.start_layers.is_empty());
        assert!(plan.stop_layers.is_empty());
        assert!(plan.unsupported_changes.is_empty());
        assert_eq!(
            plan,
            preview(&base, &candidate, &snapshot(&base), Some("dev")).unwrap()
        );
        let mut explicit_directories = candidate.clone();
        explicit_directories.services.get_mut("api").unwrap().cwd = Some("/project/api/.".into());
        assert_eq!(
            preview(&base, &explicit_directories, &snapshot(&base), Some("dev")).unwrap(),
            plan
        );
    }

    #[test]
    fn test_reload_preview_union_graph_handles_reversed_edges_without_looping() {
        let base = config("  a: {command: a}\n  b: {command: b, depends-on: [a]}\n  c: {command: c, depends-on: [b]}\n");
        let candidate = config("  a: {command: a, depends-on: [b]}\n  b: {command: b}\n  c: {command: c, depends-on: [b]}\n");
        let plan = preview(&base, &candidate, &snapshot(&base), None).unwrap();
        assert_eq!(plan.services["c"].affected_by, ["a", "b"]);
        assert_eq!(plan.stop_layers, [vec!["c"], vec!["b"], vec!["a"]]);
        assert_eq!(plan.start_layers, [vec!["b"], vec!["a", "c"]]);
    }

    #[test]
    fn test_reload_preview_rejects_invalid_graph_commands_and_platform_settings() {
        let base = config("  api: {command: worker}\n");
        for yaml in [
            "  api: {command: worker, depends-on: [absent]}\n",
            "  api: {command: worker, depends-on: [other]}\n  other: {command: worker, depends-on: [api]}\n",
            "  api: {command: \"worker 'unterminated\"}\n",
            "  api: {command: worker, restart: {initial-delay: 18446744073709551615s}}\n",
            "  api: {command: worker, healthcheck: {type: http, url: invalid}}\n",
        ] {
            assert!(preview(&base, &config(yaml), &snapshot(&base), None).is_err(), "{yaml}");
        }
        #[cfg(windows)]
        assert!(preview(
            &base,
            &config("  api: {command: worker, healthcheck: {type: socket, path: input}}\n"),
            &snapshot(&base),
            None
        )
        .is_err());
    }

    #[test]
    fn test_reload_preview_identity_tracks_configuration_instance_and_generation() {
        let base = config("  api: {command: worker, env: {TOKEN: private-value}}\n");
        let state = snapshot(&base);
        let plan = preview(&base, &base, &state, None).unwrap();
        let mut changed = state.clone();
        changed.services.get_mut("api").unwrap().event_generation = Some(2);
        let restarted = preview(&base, &base, &changed, None).unwrap();
        assert_ne!(plan.plan_id, restarted.plan_id);
        assert_eq!(plan.base_config_id, restarted.base_config_id);
        changed.event_run_id = Some("another-run".into());
        assert_ne!(
            restarted.plan_id,
            preview(&base, &base, &changed, None).unwrap().plan_id
        );
        let candidate = config("  api: {command: worker, env: {TOKEN: another-private-value}}\n");
        let changed_config = preview(&base, &candidate, &state, None).unwrap();
        assert_ne!(plan.candidate_config_id, changed_config.candidate_config_id);
        assert_eq!(changed_config.services["api"].changed_fields, ["env"]);
        let json = serde_json::to_string(&changed_config).unwrap();
        assert!(!json.contains("private-value"));
        assert!(!json.contains("TOKEN"));
    }

    #[test]
    fn test_reload_preview_detects_each_service_field() {
        let base = config("  db: {command: db}\n  api:\n    command: worker\n    depends-on: [db]\n    requires: [{type: file, path: input}]\n");
        for (field, value) in [
            ("command", "worker-new"),
            ("cwd", "/other"),
            ("env", "{MODE: changed}"),
            ("env-file", "changed.env"),
            ("depends-on", "[]"),
            ("restart-on-dep-recovery", "true"),
            ("requires", "[{type: directory, path: input}]"),
            ("monitor-requires", "true"),
            ("listen", "['127.0.0.1:32123']"),
            ("healthcheck", "{type: tcp, port: 32123}"),
            ("restart", "{policy: never}"),
            ("limits", "{memory: 1MiB}"),
        ] {
            let mut yaml = serde_yaml::to_value(&base).unwrap();
            yaml["services"]["api"][field] = serde_yaml::from_str(value).unwrap();
            let candidate: DevdConfig = serde_yaml::from_value(yaml).unwrap();
            let plan = preview(&base, &candidate, &snapshot(&base), None).unwrap();
            assert_eq!(plan.services["api"].change, ChangeKind::Modified, "{field}");
            assert!(
                plan.services["api"]
                    .changed_fields
                    .contains(&field.to_owned()),
                "{field}"
            );
            assert_eq!(plan.services["db"].change, ChangeKind::Unchanged, "{field}");
        }
    }

    #[test]
    fn test_reload_preview_preserves_symlink_parent_and_trailing_directory_semantics() {
        let base = config(
            "  api: {command: worker, cwd: /project, requires: [{type: file, path: input}]}\n",
        );
        for path in ["input/.", "input/", "link/../input"] {
            let candidate = config(&format!("  api: {{command: worker, cwd: /project, requires: [{{type: file, path: '{path}'}}]}}\n"));
            let plan = preview(&base, &candidate, &snapshot(&base), None).unwrap();
            assert_eq!(plan.services["api"].changed_fields, ["requires"], "{path}");
        }
    }

    #[test]
    fn test_reload_preview_config_serialization_preserves_scalar_durations() {
        for health in [
            "{type: script, command: 'worker check', interval: 500ms, timeout: 2s}",
            "{type: tcp, port: 1234, timeout: 400ms}",
            "{type: http, url: 'http://localhost:1234/health', interval: 1m}",
            "{type: socket, path: health.sock, interval: 500ms}",
        ] {
            let config = config(&format!("  api:\n    command: worker\n    healthcheck: {health}\n    restart: {{initial-delay: 1ms, max-delay: 18446744073709551615s}}\n"));
            let serialized = serde_yaml::to_string(&config).unwrap();
            let roundtrip: DevdConfig = serde_yaml::from_str(&serialized).unwrap();
            assert_eq!(config, roundtrip);
        }
    }
}
