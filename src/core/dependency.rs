use std::collections::BTreeMap;

use thiserror::Error;

use crate::config::{Dependency, DependencyCondition, DevdConfig};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DependencyError {
    #[error("service '{service}' references unknown dependency '{dependency}'")]
    UnknownDependency { service: String, dependency: String },
    #[error("service '{service}' declares dependency '{dependency}' more than once")]
    DuplicateDependency { service: String, dependency: String },
    #[error("circular dependency detected: {}; unresolved services: {}", .path.join(" -> "), .remaining.join(", "))]
    CircularDependency {
        path: Vec<String>,
        remaining: Vec<String>,
    },
}

/// An immutable snapshot of service dependencies, independent of process state.
#[derive(Debug, Clone)]
pub struct DependencyGraph {
    dependencies: BTreeMap<String, Vec<Dependency>>,
    dependents: BTreeMap<String, Vec<String>>,
}

impl DependencyGraph {
    /// Build the graph, rejecting unknown and duplicate references.
    /// Configuration fields and readiness probes are checked by `DevdConfig::validate`.
    /// Cycles are reported by `validate_acyclic`, `topological_order`, and `startup_layers`.
    pub fn from_config(config: &DevdConfig) -> Result<Self, DependencyError> {
        let mut dependencies: BTreeMap<_, _> = config
            .services
            .iter()
            .map(|(name, service)| (name.clone(), service.depends_on.clone()))
            .collect();
        for edges in dependencies.values_mut() {
            edges.sort_unstable_by(|left, right| left.service.cmp(&right.service));
        }
        let mut dependents: BTreeMap<String, Vec<String>> = dependencies
            .keys()
            .map(|name| (name.clone(), Vec::new()))
            .collect();
        for (name, edges) in &dependencies {
            let mut previous = None;
            for edge in edges {
                let Some(reverse) = dependents.get_mut(&edge.service) else {
                    return Err(DependencyError::UnknownDependency {
                        service: name.clone(),
                        dependency: edge.service.clone(),
                    });
                };
                if previous == Some(&edge.service) {
                    return Err(DependencyError::DuplicateDependency {
                        service: name.clone(),
                        dependency: edge.service.clone(),
                    });
                }
                reverse.push(name.clone());
                previous = Some(&edge.service);
            }
        }
        Ok(Self {
            dependencies,
            dependents,
        })
    }

    /// Service names in lexical order, including services with no edges.
    pub fn service_names(&self) -> impl Iterator<Item = &str> {
        self.dependencies.keys().map(String::as_str)
    }

    /// Direct dependencies in lexical order, with their readiness conditions.
    /// Returns `None` for an unknown service and an empty slice for a root.
    pub fn dependencies(&self, service: &str) -> Option<&[Dependency]> {
        self.dependencies.get(service).map(Vec::as_slice)
    }

    /// Services directly dependent on `service`, in lexical order.
    /// Returns `None` for an unknown service and an empty slice for a leaf.
    pub fn dependents(&self, service: &str) -> Option<&[String]> {
        self.dependents.get(service).map(Vec::as_slice)
    }

    /// Edges as `(dependent, dependency, readiness_condition)` in lexical order.
    pub fn edges(&self) -> impl Iterator<Item = (&str, &str, &DependencyCondition)> {
        self.dependencies.iter().flat_map(|(name, edges)| {
            edges
                .iter()
                .map(move |edge| (name.as_str(), edge.service.as_str(), &edge.condition))
        })
    }

    /// Deterministic dependency-first order: layers first, then lexical order.
    pub fn topological_order(&self) -> Result<Vec<String>, DependencyError> {
        let mut order = Vec::with_capacity(self.dependencies.len());
        self.walk_layers(|layer| order.extend(layer.iter().map(|name| (*name).to_owned())))?;
        Ok(order)
    }

    /// Check cycles without allocating an ordering or layer result.
    pub fn validate_acyclic(&self) -> Result<(), DependencyError> {
        self.walk_layers(|_| {})
    }

    /// Kahn layers. Services within a layer can start concurrently once their
    /// individual dependency readiness conditions are satisfied. Layer membership
    /// describes graph structure; it does not imply a running service is ready.
    pub fn startup_layers(&self) -> Result<Vec<Vec<String>>, DependencyError> {
        let mut layers = Vec::new();
        self.walk_layers(|layer| {
            layers.push(layer.iter().map(|name| (*name).to_owned()).collect());
        })?;
        Ok(layers)
    }

    fn walk_layers(&self, mut visit: impl FnMut(&[&str])) -> Result<(), DependencyError> {
        let mut remaining: BTreeMap<&str, usize> = self
            .dependencies
            .iter()
            .map(|(name, edges)| (name.as_str(), edges.len()))
            .collect();
        let mut ready: Vec<&str> = remaining
            .iter()
            .filter_map(|(&name, &count)| (count == 0).then_some(name))
            .collect();
        let mut next = Vec::new();
        while !ready.is_empty() {
            next.clear();
            for &name in &ready {
                remaining.remove(name);
                for dependent in &self.dependents[name] {
                    let count = remaining
                        .get_mut(dependent.as_str())
                        .expect("dependent of a ready service must be unresolved");
                    *count -= 1;
                    if *count == 0 {
                        next.push(dependent.as_str());
                    }
                }
            }
            visit(&ready);
            next.sort_unstable();
            std::mem::swap(&mut ready, &mut next);
        }
        if remaining.is_empty() {
            Ok(())
        } else {
            Err(self.cycle_error(&remaining))
        }
    }

    fn cycle_error(&self, remaining: &BTreeMap<&str, usize>) -> DependencyError {
        // Each unresolved node has an unresolved dependency. Follow sorted edges
        // iteratively to extract a closed cycle without including blocked leaves.
        let mut name = *remaining
            .first_key_value()
            .expect("unresolved graph is non-empty")
            .0;
        let mut path: Vec<&str> = Vec::new();
        let mut positions = BTreeMap::new();
        loop {
            if let Some(&position) = positions.get(name) {
                let mut cycle: Vec<String> = path[position..]
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect();
                cycle.push(name.to_owned());
                return DependencyError::CircularDependency {
                    path: cycle,
                    remaining: remaining.keys().map(|name| (*name).to_owned()).collect(),
                };
            }
            positions.insert(name, path.len());
            path.push(name);
            name = self.dependencies[name]
                .iter()
                .map(|edge| edge.service.as_str())
                .find(|name| remaining.contains_key(name))
                .expect("unresolved node must have an unresolved dependency");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use super::*;
    use crate::config::ConfigLoader;

    fn config(services: &str) -> DevdConfig {
        ConfigLoader::from_str(&format!("version: '1'\nservices:\n{services}"), "graph.yml")
            .unwrap()
    }

    fn graph(services: &str) -> DependencyGraph {
        DependencyGraph::from_config(&config(services)).unwrap()
    }

    #[test]
    fn test_dependency_empty_graph() {
        let graph = DependencyGraph::from_config(&DevdConfig {
            version: "1".into(),
            services: HashMap::new(),
        })
        .unwrap();
        assert!(graph.service_names().next().is_none());
        assert!(graph.edges().next().is_none());
        assert!(graph.topological_order().unwrap().is_empty());
        assert!(graph.startup_layers().unwrap().is_empty());
        graph.validate_acyclic().unwrap();
    }

    #[test]
    fn test_dependency_single_node_and_unknown_queries() {
        let graph = graph("  api: {command: api}\n");
        assert_eq!(graph.service_names().collect::<Vec<_>>(), ["api"]);
        assert_eq!(graph.topological_order().unwrap(), ["api"]);
        assert_eq!(graph.startup_layers().unwrap(), [vec!["api"]]);
        assert_eq!(graph.dependencies("api"), Some([].as_slice()));
        assert_eq!(graph.dependents("api"), Some([].as_slice()));
        assert!(graph.dependencies("missing").is_none());
        assert!(graph.dependents("missing").is_none());
    }

    #[test]
    fn test_dependency_independent_services_are_sorted() {
        let graph = graph(
            "  worker: {command: worker}\n  api: {command: api}\n  cache: {command: cache}\n",
        );
        assert_eq!(
            graph.topological_order().unwrap(),
            ["api", "cache", "worker"]
        );
        assert_eq!(
            graph.startup_layers().unwrap(),
            [vec!["api", "cache", "worker"]]
        );
    }

    #[test]
    fn test_dependency_branched_graph_has_parallel_layers_and_reverse_edges() {
        let graph = graph("  web: {command: web, depends-on: [api, worker]}\n  worker: {command: worker, depends-on: [db]}\n  api: {command: api, depends-on: [db, cache]}\n  db: {command: db}\n  cache: {command: cache}\n");
        assert_eq!(
            graph.startup_layers().unwrap(),
            [vec!["cache", "db"], vec!["api", "worker"], vec!["web"]]
        );
        assert_eq!(
            graph.topological_order().unwrap(),
            ["cache", "db", "api", "worker", "web"]
        );
        assert_eq!(graph.dependents("db").unwrap(), ["api", "worker"]);
        assert_eq!(graph.dependents("api").unwrap(), ["web"]);
        assert!(graph.dependents("web").unwrap().is_empty());
        assert_eq!(
            graph
                .dependencies("api")
                .unwrap()
                .iter()
                .map(|edge| edge.service.as_str())
                .collect::<Vec<_>>(),
            ["cache", "db"]
        );
    }

    #[test]
    fn test_dependency_edges_preserve_all_readiness_conditions() {
        let graph = graph("  db: {command: db}\n  http: {command: http}\n  tcp: {command: tcp}\n  socket: {command: socket}\n  api:\n    command: api\n    depends-on:\n      - db\n      - {service: tcp, condition: tcp-ready}\n      - {service: http, condition: http-ready}\n      - {service: socket, condition: socket-ready}\n");
        assert_eq!(
            graph.edges().collect::<Vec<_>>(),
            [
                ("api", "db", &DependencyCondition::Started),
                ("api", "http", &DependencyCondition::HttpReady),
                ("api", "socket", &DependencyCondition::SocketReady),
                ("api", "tcp", &DependencyCondition::TcpReady),
            ]
        );
        assert_eq!(
            graph.dependencies("api").unwrap()[2].condition,
            DependencyCondition::SocketReady
        );
    }

    #[test]
    fn test_dependency_rejects_unknown_and_duplicate_references() {
        assert_eq!(
            DependencyGraph::from_config(&config("  api: {command: api, depends-on: [missing]}\n"))
                .unwrap_err(),
            DependencyError::UnknownDependency {
                service: "api".into(),
                dependency: "missing".into()
            }
        );
        assert_eq!(
            DependencyGraph::from_config(&config(
                "  db: {command: db}\n  api: {command: api, depends-on: [db, db]}\n"
            ))
            .unwrap_err(),
            DependencyError::DuplicateDependency {
                service: "api".into(),
                dependency: "db".into()
            }
        );
    }

    #[test]
    fn test_dependency_cycle_reports_actual_path_and_all_unresolved_services() {
        let graph = graph("  a-blocked: {command: blocked, depends-on: [b]}\n  b: {command: b, depends-on: [c]}\n  c: {command: c, depends-on: [b]}\n  root: {command: root}\n  leaf: {command: leaf, depends-on: [root]}\n");
        let expected = DependencyError::CircularDependency {
            path: vec!["b".into(), "c".into(), "b".into()],
            remaining: vec!["a-blocked".into(), "b".into(), "c".into()],
        };
        assert_eq!(graph.startup_layers().unwrap_err(), expected);
        assert_eq!(graph.topological_order().unwrap_err(), expected);
        assert_eq!(graph.validate_acyclic().unwrap_err(), expected);
    }

    #[test]
    fn test_dependency_self_cycle() {
        let graph = graph("  api: {command: api, depends-on: [api]}\n");
        assert_eq!(
            graph.topological_order().unwrap_err(),
            DependencyError::CircularDependency {
                path: vec!["api".into(), "api".into()],
                remaining: vec!["api".into()]
            }
        );
    }

    #[test]
    fn test_dependency_is_deterministic_across_reordered_input_and_repeated_queries() {
        let original = graph("  api: {command: api, depends-on: [db, cache]}\n  db: {command: db}\n  cache: {command: cache}\n");
        let reordered = graph("  cache: {command: cache}\n  db: {command: db}\n  api: {command: api, depends-on: [cache, db]}\n");
        assert_eq!(
            original.topological_order().unwrap(),
            reordered.topological_order().unwrap()
        );
        assert_eq!(
            original.startup_layers().unwrap(),
            reordered.startup_layers().unwrap()
        );
        assert_eq!(
            original.edges().collect::<Vec<_>>(),
            reordered.edges().collect::<Vec<_>>()
        );
        for name in original.service_names() {
            assert_eq!(original.dependents(name), reordered.dependents(name));
        }
        for _ in 0..3 {
            assert_eq!(
                original.topological_order().unwrap(),
                ["cache", "db", "api"]
            );
            assert_eq!(
                original.startup_layers().unwrap(),
                [vec!["cache", "db"], vec!["api"]]
            );
        }
    }

    #[test]
    fn test_dependency_cycle_error_is_deterministic_with_multiple_cycles() {
        let original = graph("  a: {command: a, depends-on: [y, b]}\n  b: {command: b, depends-on: [c]}\n  c: {command: c, depends-on: [b]}\n  y: {command: y, depends-on: [z]}\n  z: {command: z, depends-on: [y]}\n");
        let reordered = graph("  z: {command: z, depends-on: [y]}\n  y: {command: y, depends-on: [z]}\n  c: {command: c, depends-on: [b]}\n  b: {command: b, depends-on: [c]}\n  a: {command: a, depends-on: [b, y]}\n");
        let expected = DependencyError::CircularDependency {
            path: vec!["b".into(), "c".into(), "b".into()],
            remaining: vec!["a".into(), "b".into(), "c".into(), "y".into(), "z".into()],
        };
        assert_eq!(original.validate_acyclic().unwrap_err(), expected);
        assert_eq!(reordered.topological_order().unwrap_err(), expected);
        assert_eq!(original.startup_layers().unwrap_err(), expected);
    }

    #[test]
    fn test_dependency_owns_snapshot_after_configuration_changes() {
        let mut config = config("  api: {command: api, depends-on: [db]}\n  db: {command: db}\n");
        let graph = DependencyGraph::from_config(&config).unwrap();
        config.services.clear();
        assert_eq!(graph.topological_order().unwrap(), ["db", "api"]);
        assert_eq!(graph.dependents("db").unwrap(), ["api"]);
    }

    #[test]
    fn test_dependency_all_three_node_graphs_match_reachability_oracle() {
        let names = ["a", "b", "c"];
        let template = config("  base: {command: base}\n")
            .services
            .remove("base")
            .unwrap();
        for mask in 0..512u16 {
            let mut config = DevdConfig {
                version: "1".into(),
                services: HashMap::new(),
            };
            let mut reachable = [[false; 3]; 3];
            for (source, name) in names.iter().enumerate() {
                let mut service = template.clone();
                for (target, dependency) in names.iter().enumerate() {
                    if mask & (1 << (source * 3 + target)) != 0 {
                        reachable[source][target] = true;
                        service.depends_on.push(Dependency {
                            service: (*dependency).into(),
                            condition: DependencyCondition::Started,
                        });
                    }
                }
                config.services.insert((*name).into(), service);
            }
            for via in 0..3 {
                for source in 0..3 {
                    for target in 0..3 {
                        reachable[source][target] |=
                            reachable[source][via] && reachable[via][target];
                    }
                }
            }
            let cyclic = (0..3).any(|index| reachable[index][index]);
            let graph = DependencyGraph::from_config(&config).unwrap();
            assert_eq!(graph.validate_acyclic().is_err(), cyclic, "graph {mask}");
            if cyclic {
                let DependencyError::CircularDependency { path, remaining } =
                    graph.topological_order().unwrap_err()
                else {
                    panic!("expected a cycle for graph {mask}");
                };
                assert_eq!(path.first(), path.last(), "graph {mask}");
                for edge in path.windows(2) {
                    assert!(
                        graph
                            .dependencies(&edge[0])
                            .unwrap()
                            .iter()
                            .any(|dependency| dependency.service == edge[1]),
                        "invalid cycle edge for graph {mask}"
                    );
                }
                let expected_remaining: Vec<_> = (0..3)
                    .filter(|&source| {
                        (0..3).any(|target| reachable[source][target] && reachable[target][target])
                    })
                    .map(|index| names[index].to_owned())
                    .collect();
                assert_eq!(remaining, expected_remaining, "graph {mask}");
            } else {
                let order = graph.topological_order().unwrap();
                assert_eq!(
                    order.iter().map(String::as_str).collect::<BTreeSet<_>>(),
                    BTreeSet::from(names)
                );
                assert_eq!(order.len(), names.len());
                let positions: BTreeMap<_, _> = order
                    .iter()
                    .enumerate()
                    .map(|(index, name)| (name.as_str(), index))
                    .collect();
                let layers = graph.startup_layers().unwrap();
                assert_eq!(
                    layers.iter().flatten().collect::<Vec<_>>(),
                    order.iter().collect::<Vec<_>>()
                );
                let layer_indices: BTreeMap<_, _> = layers
                    .iter()
                    .enumerate()
                    .flat_map(|(index, layer)| layer.iter().map(move |name| (name.as_str(), index)))
                    .collect();
                for (dependent, dependency, _) in graph.edges() {
                    assert!(positions[dependency] < positions[dependent], "graph {mask}");
                    assert!(
                        layer_indices[dependency] < layer_indices[dependent],
                        "graph {mask}"
                    );
                    assert!(graph
                        .dependents(dependency)
                        .unwrap()
                        .iter()
                        .any(|name| name == dependent));
                }
                for layer in layers {
                    assert!(layer.windows(2).all(|names| names[0] < names[1]));
                }
            }
        }
    }
}
