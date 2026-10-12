//! Historical declared relationships. Capture uses one bounded observation batch;
//! readers project only saved facts and never load configuration or inspect paths.
use super::evidence::{self, Configuration, Resource};
use crate::{
    config::{Dependency, DependencyCondition, DevdConfig},
    core::{
        dependency::DependencyGraph,
        resource_impact::{self, MatchBasis, View},
        resource_inspection,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::{Path, PathBuf},
};

pub(super) const MAX_BYTES: usize = 512 * 1024;
const EDGE_BYTES: usize = 96 * 1024;
const IMPACT_BYTES: usize = 256 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(in crate::cli) struct Edge {
    pub dependent: String,
    pub dependency: String,
    pub condition: DependencyCondition,
}
#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Impact {
    pub observation_index: usize,
    pub view: View,
}
#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Graph {
    pub config_observed_at: DateTime<Utc>,
    pub observed_from: DateTime<Utc>,
    pub observed_to: DateTime<Utc>,
    pub source: String,
    pub atomic: bool,
    pub trigger_time_equivalence: String,
    pub services: Vec<String>,
    /// Direction is consumer -> dependency, as in depends-on.
    pub dependencies: Vec<Edge>,
    pub omitted_services: usize,
    pub omitted_dependencies: usize,
    pub impacts: Vec<Impact>,
    pub unavailable_impacts: usize,
    pub omitted_impacts: usize,
    pub inspections_performed: usize,
    pub interrupted: bool,
}

fn edges(config: &Configuration) -> Vec<Edge> {
    let names: BTreeSet<_> = config.services.iter().map(|s| &s.name).collect();
    let mut result = vec![];
    let mut bytes = 0;
    for service in &config.services {
        let mut deps: Vec<_> = service.dependencies.iter().collect();
        deps.sort_by(|a, b| a.service.cmp(&b.service));
        for dependency in deps {
            if !names.contains(&dependency.service) {
                continue;
            }
            let edge = Edge {
                dependent: service.name.clone(),
                dependency: dependency.service.clone(),
                condition: dependency.condition.clone(),
            };
            let size = serde_json::to_vec(&edge).expect("edge").len();
            if result.len() == resource_impact::MAX_EDGES || bytes + size > EDGE_BYTES {
                return result;
            }
            bytes += size;
            result.push(edge);
        }
    }
    result
}

pub(super) fn capture(
    config: &DevdConfig,
    state: &Path,
    observations: &[Resource],
    config_time: DateTime<Utc>,
    mut stopped: impl FnMut() -> bool,
) -> Graph {
    let summary = evidence::configuration(config);
    let dependencies = edges(&summary);
    let total = config
        .services
        .values()
        .map(|s| s.depends_on.len())
        .sum::<usize>();
    let mut graph = Graph {
        config_observed_at: config_time,
        observed_from: Utc::now(),
        observed_to: Utc::now(),
        source: "effective-config-and-post-trigger-declaration-observations".into(),
        atomic: false,
        trigger_time_equivalence: "not-guaranteed".into(),
        services: summary.services.iter().map(|s| s.name.clone()).collect(),
        omitted_services: summary.omitted_services,
        omitted_dependencies: total.saturating_sub(dependencies.len()),
        dependencies,
        impacts: vec![],
        unavailable_impacts: 0,
        omitted_impacts: 0,
        inspections_performed: 0,
        interrupted: false,
    };
    let mut cache: BTreeMap<PathBuf, resource_inspection::Inspection> = BTreeMap::new();
    let mut bytes = 0;
    for (index, observation) in observations.iter().enumerate() {
        if stopped() {
            graph.interrupted = true;
            graph.unavailable_impacts += observations.len() - index;
            break;
        }
        if graph.omitted_services > 0 || graph.omitted_dependencies > 0 {
            graph.unavailable_impacts += 1;
            continue;
        }
        let view = resource_impact::analyze_with(config, state, &observation.target, |path| {
            if stopped() {
                graph.interrupted = true;
                return None;
            }
            if let Some(existing) = cache.get(path) {
                return Some(existing.clone());
            }
            if cache.len() == resource_impact::MAX_DECLARATIONS {
                return None;
            }
            let observed = resource_inspection::inspect(path);
            cache.insert(path.into(), observed.clone());
            Some(observed)
        });
        match view {
            Ok(view) => {
                let impact = Impact {
                    observation_index: index,
                    view,
                };
                let size = serde_json::to_vec(&impact).expect("impact").len();
                if bytes + size > IMPACT_BYTES {
                    graph.omitted_impacts += 1;
                } else {
                    bytes += size;
                    graph.impacts.push(impact);
                }
            }
            Err(_) => graph.unavailable_impacts += 1,
        }
    }
    graph.inspections_performed = cache.len();
    graph.observed_to = Utc::now();
    // Keep the relationship section independently bounded before the artifact's
    // broader 2 MiB cap. Dropped views remain visible as coverage omissions.
    while serde_json::to_vec(&graph).is_ok_and(|bytes| bytes.len() > MAX_BYTES) {
        if graph.impacts.pop().is_none() {
            break;
        }
        graph.omitted_impacts += 1;
    }
    graph
}

impl Graph {
    pub(super) fn complete(&self) -> bool {
        self.omitted_services == 0
            && self.omitted_dependencies == 0
            && self.unavailable_impacts == 0
            && self.omitted_impacts == 0
            && !self.interrupted
            && self.impacts.iter().all(|i| i.view.complete)
    }

    fn dependency_graph(&self) -> Option<DependencyGraph> {
        let mut map: BTreeMap<_, Vec<Dependency>> =
            self.services.iter().map(|s| (s.clone(), vec![])).collect();
        for edge in &self.dependencies {
            map.get_mut(&edge.dependent)?.push(Dependency {
                service: edge.dependency.clone(),
                condition: edge.condition.clone(),
            });
        }
        DependencyGraph::from_dependencies(map).ok()
    }

    pub(super) fn validate(&mut self, config: &Configuration, observations: &[Resource]) -> bool {
        if serde_json::to_vec(self).map_or(true, |s| s.len() > MAX_BYTES)
            || self.services.len() > resource_impact::MAX_SERVICES
            || self.dependencies.len() > resource_impact::MAX_EDGES
            || self.inspections_performed > resource_impact::MAX_DECLARATIONS
            || self.impacts.len() > observations.len()
            || self.services
                != config
                    .services
                    .iter()
                    .map(|s| s.name.clone())
                    .collect::<Vec<_>>()
            || self.omitted_services != config.omitted_services
            || self.dependencies != edges(config)
            || config.observed_at != Some(self.config_observed_at)
            || self
                .impacts
                .len()
                .checked_add(self.unavailable_impacts)
                .and_then(|n| n.checked_add(self.omitted_impacts))
                != Some(observations.len())
        {
            return false;
        }
        let Some(known_dependencies) = config.services.iter().try_fold(0usize, |total, s| {
            total
                .checked_add(s.dependencies.len())?
                .checked_add(s.omitted_dependencies)
        }) else {
            return false;
        };
        if self.omitted_dependencies < known_dependencies.saturating_sub(self.dependencies.len()) {
            return false;
        }
        let Some(graph) = self.dependency_graph() else {
            return false;
        };
        if graph.validate_acyclic().is_err() {
            return false;
        }
        let mut indexes = BTreeSet::new();
        for impact in &mut self.impacts {
            let Some(selected) = observations.get(impact.observation_index) else {
                return false;
            };
            let view = &mut impact.view;
            if !indexes.insert(impact.observation_index)
                || view.matches.len() > resource_impact::MAX_MATCHES
                || view.service_nodes != self.services.len()
                || view.dependency_edges != self.dependencies.len()
                || self.omitted_services > 0
                || self.omitted_dependencies > 0
            {
                return false;
            }
            let mut declarations = BTreeSet::new();
            for entry in &view.matches {
                let Some(service) = config
                    .services
                    .iter()
                    .find(|s| s.name == entry.selector.service)
                else {
                    return false;
                };
                if !declarations.insert(&entry.selector) {
                    return false;
                }
                match &entry.selector.field {
                    resource_impact::Field::Requires { index }
                        if *index >= service.requires_count || entry.declared_scope.is_some() =>
                    {
                        return false
                    }
                    resource_impact::Field::Paths { name }
                        if name.len() > 4096
                            || service.paths_count == 0
                            || entry.declared_scope.is_none() =>
                    {
                        return false
                    }
                    _ => {}
                }
                if entry.declared_path.as_os_str().len() > resource_inspection::MAX_PATH_BYTES
                    || entry.requested_path.as_os_str().len() > resource_inspection::MAX_PATH_BYTES
                    || entry
                        .observed_target
                        .as_ref()
                        .is_some_and(|s| s.len() > resource_inspection::MAX_PATH_BYTES)
                {
                    return false;
                }
                let same_request = entry.requested_path == selected.target.requested_path;
                let same_target = selected.target.inspection.complete
                    && selected.target.inspection.failure.is_none()
                    && entry.inspection_complete
                    && entry.observed_failure.is_none()
                    && entry.observed_target.is_some()
                    && entry.observed_target == selected.target.inspection.resolved;
                if match entry.basis {
                    MatchBasis::SameRequestedPath => !same_request,
                    MatchBasis::SameObservedTarget => same_request || !same_target,
                } {
                    return false;
                }
                let conflict = same_request
                    && (entry.observed_target != selected.target.inspection.resolved
                        || entry.observed_failure != selected.target.inspection.failure);
                if entry.conflicts_with_selection != conflict {
                    return false;
                }
            }
            // Serialized derived fields are not an authority: regenerate them from
            // the retained graph/matches, without touching the current filesystem.
            resource_impact::project(view, &graph);
            view.complete = view.complete
                && selected.target.inspection.complete
                && view.omitted_declarations == 0
                && view.omitted_matches == 0
                && view.uncertain_aliases == 0
                && view
                    .matches
                    .iter()
                    .all(|m| m.inspection_complete && !m.conflicts_with_selection);
        }
        self.source = "effective-config-and-post-trigger-declaration-observations".into();
        self.atomic = false;
        self.trigger_time_equivalence = "not-guaranteed".into();
        serde_json::to_vec(self).is_ok_and(|s| s.len() <= MAX_BYTES)
    }

    /// A bounded tree projection: one expansion per node, shared/cyclic edges
    /// become references. Stored edges always retain every recorded parent.
    pub(super) fn render(&self, trigger: &str, observations: &[Resource]) -> String {
        let Some(graph) = self.dependency_graph() else {
            return "Declared relationships unavailable.\n".into();
        };
        const DISPLAY_BYTES: usize = 128 * 1024;
        let mut text = String::from("Recorded declared relationships (not proven causation):\n");
        let mut omitted_lines = 0usize;
        let mut append = |line: String| {
            if text.len() + line.len() <= DISPLAY_BYTES {
                text.push_str(&line);
            } else {
                omitted_lines += 1;
            }
        };
        let forests = std::iter::once((None, BTreeSet::from([trigger.to_owned()]))).chain(
            self.impacts.iter().map(|impact| {
                (
                    Some(impact),
                    impact
                        .view
                        .direct_consumers
                        .iter()
                        .map(|c| c.service.clone())
                        .collect(),
                )
            }),
        );
        for (impact, roots) in forests {
            if let Some(impact) = impact {
                let selected = observations.get(impact.observation_index);
                append(format!(
                    "resource observation #{}: {}\n",
                    impact.observation_index,
                    serde_json::to_string(&selected.map(|r| &r.target.requested_path))
                        .expect("path")
                ));
                for entry in &impact.view.matches {
                    append(format!(
                        "  match {} via {} (scope={})\n",
                        serde_json::to_string(&entry.selector).expect("selector"),
                        serde_json::to_string(&entry.basis).expect("basis"),
                        serde_json::to_string(&entry.declared_scope).expect("scope")
                    ));
                }
                append("  declared users and downstream:\n".into());
            } else {
                append("trigger service and declared downstream:\n".into());
            }
            let mut seen = BTreeSet::new();
            let mut stack: Vec<_> = roots.into_iter().rev().map(|s| (s, 0, None)).collect();
            while let Some((service, depth, condition)) = stack.pop() {
                let first = seen.insert(service.clone());
                let gate = condition.map_or(String::new(), |c: DependencyCondition| {
                    format!(
                        " (depends-on: {})",
                        serde_json::to_string(&c).expect("condition")
                    )
                });
                append(format!(
                    "{}+- {}{}{}\n",
                    "  ".repeat(depth + 1),
                    serde_json::to_string(&service).expect("service"),
                    gate,
                    if first { "" } else { " [ref]" }
                ));
                if first {
                    if let Some(children) = graph.dependents(&service) {
                        stack.extend(children.iter().rev().map(|s| {
                            let condition = graph
                                .dependencies(s)
                                .and_then(|edges| edges.iter().find(|e| e.service == service))
                                .map(|e| e.condition.clone());
                            (s.clone(), depth + 1, condition)
                        }));
                    }
                }
            }
        }
        if omitted_lines > 0 {
            writeln!(
                text,
                "  Display omitted {omitted_lines} lines; use --json for the retained graph."
            )
            .unwrap();
        }
        if !self.complete() {
            text.push_str("  Coverage incomplete; inspect recorded omissions/unknown aliases.\n");
        }
        text
    }
}

#[cfg(test)]
mod tests;
