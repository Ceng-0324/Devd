//! Declared path use and dependency reachability. No process control or ownership inference.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    config::{DevdConfig, PathScope},
    core::{dependency::DependencyGraph, path_requirements, resource_inspection},
};

pub const MAX_SERVICES: usize = 256;
pub const MAX_EDGES: usize = 1024;
pub const MAX_DECLARATIONS: usize = 256;
pub const MAX_MATCHES: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "field", rename_all = "kebab-case")]
pub enum Field {
    Requires { index: usize },
    Paths { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Selector {
    pub service: String,
    #[serde(flatten)]
    pub field: Field,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub selector: Selector,
    pub declared_path: PathBuf,
    pub requested_path: PathBuf,
    /// A requires declaration has no ownership scope, even if its path aliases a binding.
    pub declared_scope: Option<PathScope>,
    pub inspection: resource_inspection::Inspection,
    pub risk_hints: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchBasis {
    SameRequestedPath,
    SameObservedTarget,
}

#[derive(Debug, Serialize)]
pub struct Match {
    pub selector: Selector,
    pub declared_path: PathBuf,
    pub requested_path: PathBuf,
    pub declared_scope: Option<PathScope>,
    pub basis: MatchBasis,
    pub observed_target: Option<String>,
    pub inspection_complete: bool,
    pub observed_failure: Option<resource_inspection::Failure>,
    pub conflicts_with_selection: bool,
}

#[derive(Debug, Serialize)]
pub struct ConsumerReason {
    pub declaration: Selector,
    /// `requires`, `binding-provider`, or `binding-recipient`.
    pub relation: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Consumer {
    pub service: String,
    pub reasons: Vec<ConsumerReason>,
}

#[derive(Debug, Serialize)]
pub struct Downstream {
    pub service: String,
    /// First element is a direct declared user; subsequent edges are service dependencies.
    pub via_services: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct View {
    pub complete: bool,
    pub omitted_declarations: usize,
    pub omitted_matches: usize,
    pub uncertain_aliases: usize,
    pub service_nodes: usize,
    pub dependency_edges: usize,
    pub matches: Vec<Match>,
    pub direct_consumers: Vec<Consumer>,
    pub downstream: Vec<Downstream>,
}

pub fn select(config: &DevdConfig, state_dir: &Path, selector: Selector) -> Result<Target> {
    let service = config
        .services
        .get(&selector.service)
        .with_context(|| format!("unknown service {:?}", selector.service))?;
    let (declared_path, requested_path, declared_scope) = match &selector.field {
        Field::Requires { index } => {
            let requirement = service.requires.get(*index).with_context(|| {
                format!("service {:?} has no requires[{index}]", selector.service)
            })?;
            (
                requirement.path.clone(),
                path_requirements::resolve_path(&requirement.path, service.cwd.as_deref()),
                None,
            )
        }
        Field::Paths { name } => {
            let binding = service
                .paths
                .get(name)
                .with_context(|| format!("service {:?} has no paths.{name}", selector.service))?;
            (
                binding.path.clone(),
                binding.resolve(service.cwd.as_deref(), state_dir)?,
                Some(binding.scope),
            )
        }
    };
    if requested_path.as_os_str().len() > resource_inspection::MAX_PATH_BYTES {
        bail!(
            "selected path exceeds the {}-byte inspection limit",
            resource_inspection::MAX_PATH_BYTES
        );
    }
    let inspection = resource_inspection::inspect(&requested_path);
    let temp = std::env::temp_dir();
    let risky = [
        &requested_path,
        &PathBuf::from(inspection.resolved.as_deref().unwrap_or("")),
    ]
    .iter()
    .any(|path| {
        path.starts_with(&temp)
            || path.starts_with(Path::new("/tmp"))
            || path.starts_with(Path::new("/private/tmp"))
    });
    Ok(Target {
        selector,
        declared_path,
        requested_path,
        declared_scope,
        inspection,
        risk_hints: if risky {
            vec!["temporary-location-is-not-deletion-authorization".into()]
        } else {
            vec![]
        },
    })
}

/// Assess one effective on-disk configuration. For peer instances the caller
/// must label this a candidate: a live identity does not prove YAML matches memory.
pub fn analyze(config: &DevdConfig, state_dir: &Path, target: &Target) -> Result<View> {
    let service_nodes = config.services.len();
    let dependency_edges = config
        .services
        .values()
        .map(|service| service.depends_on.len())
        .sum::<usize>();
    if service_nodes > MAX_SERVICES || dependency_edges > MAX_EDGES {
        bail!(
            "impact graph exceeds limits ({MAX_SERVICES} services, {MAX_EDGES} dependency edges)"
        );
    }
    let graph = DependencyGraph::from_config(config)?;
    let mut view = View {
        complete: target.inspection.complete,
        omitted_declarations: 0,
        omitted_matches: 0,
        uncertain_aliases: 0,
        service_nodes,
        dependency_edges,
        matches: vec![],
        direct_consumers: vec![],
        downstream: vec![],
    };
    let mut direct: BTreeMap<String, BTreeSet<(Selector, &'static str)>> = BTreeMap::new();
    let mut examined = 0;
    for service in graph.service_names() {
        let definition = &config.services[service];
        let remaining = MAX_DECLARATIONS - examined;
        let omitted =
            (definition.requires.len() + definition.paths.len()).saturating_sub(remaining);
        view.omitted_declarations += omitted;
        if omitted > 0 {
            view.complete = false;
        }
        let requires = definition
            .requires
            .iter()
            .enumerate()
            .map(|(index, requirement)| {
                (
                    Field::Requires { index },
                    requirement.path.clone(),
                    None,
                    Ok(path_requirements::resolve_path(
                        &requirement.path,
                        definition.cwd.as_deref(),
                    )),
                )
            });
        let paths = definition.paths.iter().map(|(name, binding)| {
            (
                Field::Paths { name: name.clone() },
                binding.path.clone(),
                Some(binding.scope),
                binding.resolve(definition.cwd.as_deref(), state_dir),
            )
        });
        for (field, declared_path, scope, requested) in requires.chain(paths).take(remaining) {
            examined += 1;
            let Ok(requested_path) = requested else {
                view.uncertain_aliases += 1;
                view.complete = false;
                continue;
            };
            if requested_path.as_os_str().len() > resource_inspection::MAX_PATH_BYTES {
                view.uncertain_aliases += 1;
                view.complete = false;
                continue;
            }
            let observation = resource_inspection::inspect(&requested_path);
            let same_request = requested_path == target.requested_path;
            let same_target = target.inspection.complete
                && target.inspection.failure.is_none()
                && observation.complete
                && observation.failure.is_none()
                && target.inspection.resolved.is_some()
                && target.inspection.resolved == observation.resolved;
            if !same_request && !same_target {
                if observation.failure.is_some()
                    || !observation.complete
                    || target.inspection.failure.is_some()
                    || !target.inspection.complete
                {
                    view.uncertain_aliases += 1;
                    view.complete = false;
                }
                continue;
            }
            if view.matches.len() == MAX_MATCHES {
                view.omitted_matches += 1;
                view.complete = false;
                continue;
            }
            let selector = Selector {
                service: service.to_owned(),
                field,
            };
            let basis = if same_request {
                MatchBasis::SameRequestedPath
            } else {
                MatchBasis::SameObservedTarget
            };
            let match_entry = Match {
                selector,
                declared_path,
                requested_path,
                declared_scope: scope,
                basis,
                conflicts_with_selection: same_request
                    && (observation.resolved != target.inspection.resolved
                        || observation.failure != target.inspection.failure),
                observed_failure: observation.failure,
                observed_target: observation.resolved,
                inspection_complete: observation.complete,
            };
            let reason = match_entry.selector.clone();
            match &match_entry.selector.field {
                Field::Requires { .. } => {
                    direct
                        .entry(service.to_owned())
                        .or_default()
                        .insert((reason, "requires"));
                }
                Field::Paths { .. } => {
                    direct
                        .entry(service.to_owned())
                        .or_default()
                        .insert((reason.clone(), "binding-provider"));
                    for recipient in graph.dependents(service).expect("graph service exists") {
                        direct
                            .entry(recipient.clone())
                            .or_default()
                            .insert((reason.clone(), "binding-recipient"));
                    }
                }
            }
            if !match_entry.inspection_complete || match_entry.conflicts_with_selection {
                view.complete = false;
            }
            view.matches.push(match_entry);
        }
    }
    let mut direct_set = BTreeSet::new();
    for (service, reasons) in direct {
        direct_set.insert(service.clone());
        view.direct_consumers.push(Consumer {
            service,
            reasons: reasons
                .into_iter()
                .map(|(declaration, relation)| ConsumerReason {
                    declaration,
                    relation,
                })
                .collect(),
        });
    }
    // A deterministic shortest witness path, bounded by the checked graph.
    let mut queue: VecDeque<Vec<String>> =
        direct_set.iter().map(|name| vec![name.clone()]).collect();
    let mut seen = direct_set;
    while let Some(path) = queue.pop_front() {
        for next in graph
            .dependents(path.last().expect("path has a service"))
            .expect("graph service exists")
        {
            if seen.insert(next.clone()) {
                let mut via_services = path.clone();
                via_services.push(next.clone());
                queue.push_back(via_services.clone());
                view.downstream.push(Downstream {
                    service: next.clone(),
                    via_services,
                });
            }
        }
    }
    view.downstream
        .sort_by(|left, right| left.service.cmp(&right.service));
    Ok(view)
}

#[cfg(test)]
mod tests;
