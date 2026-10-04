use std::path::Path;

use devd::config::{ConfigLoader, DependencyCondition};
use devd::core::dependency::DependencyGraph;

#[tokio::test]
async fn test_dependency_graph_from_validated_file_preserves_readiness_and_order() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid-config.yml");
    let config = ConfigLoader::new().load(path).await.unwrap();
    let graph = DependencyGraph::from_config(&config).unwrap();
    assert_eq!(graph.topological_order().unwrap(), ["db", "api"]);
    assert_eq!(graph.startup_layers().unwrap(), [vec!["db"], vec!["api"]]);
    assert_eq!(
        graph.edges().collect::<Vec<_>>(),
        [("api", "db", &DependencyCondition::TcpReady)]
    );
    assert_eq!(graph.dependents("db").unwrap(), ["api"]);
}
