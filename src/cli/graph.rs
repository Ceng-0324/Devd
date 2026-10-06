use std::{collections::BTreeMap, fmt::Write};

use anyhow::Result;
use clap::ValueEnum;

use crate::{
    config::{DependencyCondition, DevdConfig},
    core::dependency::DependencyGraph,
};

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum GraphFormat {
    Text,
    Dot,
    Mermaid,
}

pub(super) fn render(config: &DevdConfig, format: GraphFormat) -> Result<String> {
    let graph = DependencyGraph::from_config(config)?;
    match format {
        GraphFormat::Text => render_text(&graph),
        GraphFormat::Dot => Ok(render_dot(&graph)),
        GraphFormat::Mermaid => Ok(render_mermaid(&graph)),
    }
}

fn render_text(graph: &DependencyGraph) -> Result<String> {
    let mut text = String::from("Dependencies (service -> prerequisite):\n");
    for name in graph.service_names() {
        let dependencies = graph.dependencies(name).expect("service is in graph");
        if dependencies.is_empty() {
            writeln!(text, "  {name}")?;
        }
        for dependency in dependencies {
            writeln!(
                text,
                "  {name} -> {} ({:?})",
                dependency.service, dependency.condition
            )?;
        }
    }
    text.push_str("Startup layers:\n");
    for (index, layer) in graph.startup_layers()?.iter().enumerate() {
        writeln!(text, "  {}: {}", index + 1, layer.join(", "))?;
    }
    Ok(text)
}

fn render_dot(graph: &DependencyGraph) -> String {
    let mut text = String::from("digraph devd {\n  rankdir=LR;\n");
    for name in graph.service_names() {
        writeln!(text, "  \"{name}\";").expect("writing to String cannot fail");
    }
    for (dependent, prerequisite, condition) in graph.edges() {
        writeln!(
            text,
            "  \"{prerequisite}\" -> \"{dependent}\" [label=\"{}\"];",
            condition_label(condition)
        )
        .expect("writing to String cannot fail");
    }
    text.push_str("}\n");
    text
}

fn render_mermaid(graph: &DependencyGraph) -> String {
    let names: Vec<_> = graph.service_names().collect();
    let ids: BTreeMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(id, &name)| (name, id))
        .collect();
    let mut text = String::from("flowchart LR\n");
    for (id, name) in names.iter().enumerate() {
        writeln!(text, "  s{id}[\"{name}\"]").expect("writing to String cannot fail");
    }
    for (dependent, prerequisite, condition) in graph.edges() {
        writeln!(
            text,
            "  s{} -->|{}| s{}",
            ids[prerequisite],
            condition_label(condition),
            ids[dependent]
        )
        .expect("writing to String cannot fail");
    }
    text
}

fn condition_label(condition: &DependencyCondition) -> &'static str {
    match condition {
        DependencyCondition::Started => "started",
        DependencyCondition::SocketReady => "socket-ready",
        DependencyCondition::TcpReady => "tcp-ready",
        DependencyCondition::HttpReady => "http-ready",
    }
}
