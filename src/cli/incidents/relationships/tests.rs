use super::*;
use crate::{
    config::ConfigLoader,
    core::resource_impact::{Field, Selector},
};

fn fixture() -> (tempfile::TempDir, DevdConfig, Vec<Resource>) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("data"), "SECRET").unwrap();
    let config = ConfigLoader::from_str("version: '1'\nservices:\n  db: {command: SECRET, requires: [{type: file, path: data}]}\n  api: {command: SECRET, depends-on: [db]}\n  worker: {command: SECRET, depends-on: [db]}\n  web: {command: SECRET, depends-on: [api, worker]}\n", root.path().join("devd.yml")).unwrap();
    let resource = Resource {
        target: resource_impact::select(
            &config,
            root.path(),
            Selector {
                service: "db".into(),
                field: Field::Requires { index: 0 },
            },
        )
        .unwrap(),
        requirement_type: Some(crate::config::PathRequirementType::File),
        declared_cleanup: None,
    };
    (root, config, vec![resource])
}

#[test]
fn test_relationship_diamond_retains_both_parents_and_renders_shared_reference() {
    let (root, config, resources) = fixture();
    let now = Utc::now();
    let mut graph = capture(&config, root.path(), &resources, now, || false);
    assert_eq!(graph.dependencies.len(), 4);
    assert!(graph.complete());
    assert_eq!(graph.impacts[0].view.downstream.len(), 3);
    let text = graph.render("db", &resources);
    assert!(text.contains("[ref]"));
    assert!(!text.contains("SECRET"));
    assert!(text.contains("+- \"db\""));
    let mut summary = evidence::configuration(&config);
    summary.observed_at = Some(now);
    std::fs::remove_file(root.path().join("data")).unwrap();
    assert!(graph.validate(&summary, &resources));
    assert_eq!(graph.render("db", &resources), text);
    graph.dependencies.pop();
    assert!(!graph.validate(&summary, &resources));
}

#[test]
fn test_relationship_cancellation_and_bounded_shared_inspection_batch() {
    let (root, mut config, mut resources) = fixture();
    let mut count = 0;
    let graph = capture(&config, root.path(), &resources, Utc::now(), || {
        count += 1;
        count >= 2
    });
    assert!(graph.interrupted && !graph.complete());
    assert_eq!(graph.inspections_performed, 0);
    let requirement = config.services["db"].requires[0].clone();
    config.services.get_mut("db").unwrap().requires = vec![requirement; 257];
    resources.push(Resource {
        target: resources[0].target.clone(),
        requirement_type: resources[0].requirement_type,
        declared_cleanup: None,
    });
    let graph = capture(&config, root.path(), &resources, Utc::now(), || false);
    assert_eq!(graph.inspections_performed, 1);
    assert_eq!(graph.impacts.len(), 2);
    assert!(graph
        .impacts
        .iter()
        .all(|i| i.view.omitted_declarations == 1 && i.view.omitted_matches == 192));
    assert!(!graph.complete());
}

#[test]
fn test_relationship_validation_rejects_forged_match_and_reprojects_derived_fields() {
    let (root, config, resources) = fixture();
    let now = Utc::now();
    let mut summary = evidence::configuration(&config);
    summary.observed_at = Some(now);
    let mut graph = capture(&config, root.path(), &resources, now, || false);
    graph.impacts[0].view.downstream.clear();
    graph.source = "SECRET".into();
    graph.atomic = true;
    assert!(graph.validate(&summary, &resources));
    assert_eq!(graph.impacts[0].view.downstream.len(), 3);
    assert!(!graph.atomic && !graph.source.contains("SECRET"));
    graph.impacts[0].view.matches[0].requested_path = root.path().join("forged");
    assert!(!graph.validate(&summary, &resources));
}

#[test]
fn test_relationship_capacity_gaps_remain_valid_partial_history() {
    let (root, mut config, resources) = fixture();
    let template = config.services["api"].clone();
    for (services, dense) in [(257, false), (66, true)] {
        config.services.clear();
        for index in 0..services {
            let mut service = template.clone();
            service.depends_on = if dense {
                (0..index.min(64))
                    .map(|dependency| Dependency {
                        service: format!("node{dependency:03}"),
                        condition: DependencyCondition::Started,
                    })
                    .collect()
            } else {
                vec![]
            };
            config.services.insert(format!("node{index:03}"), service);
        }
        let now = Utc::now();
        let mut summary = evidence::configuration(&config);
        summary.observed_at = Some(now);
        let mut graph = capture(&config, root.path(), &resources, now, || false);
        assert!(!graph.complete());
        assert!(graph.impacts.is_empty());
        assert_eq!(graph.unavailable_impacts, 1);
        assert_eq!(graph.inspections_performed, 0);
        assert!(serde_json::to_vec(&graph).unwrap().len() <= MAX_BYTES);
        if dense {
            assert_eq!(graph.dependencies.len(), resource_impact::MAX_EDGES);
            assert!(graph.omitted_dependencies > 0);
        } else {
            assert_eq!(graph.services.len(), resource_impact::MAX_SERVICES);
            assert_eq!(graph.omitted_services, 1);
        }
        assert!(graph.validate(&summary, &resources));
    }
}

#[test]
fn test_relationship_reader_rejects_cycles_even_when_summary_and_edges_agree() {
    let (root, mut config, _) = fixture();
    config.services.get_mut("db").unwrap().depends_on = vec![Dependency {
        service: "web".into(),
        condition: DependencyCondition::Started,
    }];
    let now = Utc::now();
    let mut summary = evidence::configuration(&config);
    summary.observed_at = Some(now);
    let mut graph = capture(&config, root.path(), &[], now, || false);
    assert!(!graph.validate(&summary, &[]));
}
