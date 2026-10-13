use super::*;
use crate::{
    config::DevdConfig,
    core::{
        dependency::DependencyGraph,
        path_requirements, resource_impact,
        resource_inspection::{self, Evidence},
    },
};
use std::collections::{BTreeMap, VecDeque};

pub(super) fn validate_graph(c: &DevdConfig) -> Result<()> {
    if c.services.len() > resource_impact::MAX_SERVICES
        || c.services
            .values()
            .map(|s| s.depends_on.len())
            .sum::<usize>()
            > resource_impact::MAX_EDGES
    {
        bail!("baseline dependency graph exceeds 256 services or 1024 edges");
    }
    DependencyGraph::from_config(c)?.validate_acyclic()?;
    Ok(())
}
pub(super) fn resolve(
    c: &DevdConfig,
    state: &Path,
    service: &str,
    selection: &Selection,
) -> Result<Declaration> {
    let s = c
        .services
        .get(service)
        .context("selected service is unavailable")?;
    let cwd = s.cwd.as_deref().context("service cwd is unresolved")?;
    let (declared, requested, requirement, scope, cleanup) = match selection {
        Selection::Requires { index } => {
            let r = s
                .requires
                .get(*index)
                .context("selected requires index is unavailable")?;
            (
                r.path.clone(),
                path_requirements::resolve_path(&r.path, Some(cwd)),
                Some(r.kind),
                None,
                false,
            )
        }
        Selection::Paths { name } => {
            let r = s
                .paths
                .get(name)
                .context("selected path binding is unavailable")?;
            (
                r.path.clone(),
                r.resolve(Some(cwd), state)?,
                None,
                Some(r.scope),
                r.cleanup,
            )
        }
        Selection::Cwd => (cwd.to_owned(), cwd.to_owned(), None, None, false),
        Selection::EnvFile => {
            let p = s.env_file.as_ref().context("env-file is not declared")?;
            (p.clone(), cwd.join(p), None, None, false)
        }
        Selection::Program => {
            let words = shell_words::split(&s.command).context("invalid service command")?;
            let first = words.first().context("service command is empty")?;
            let p = PathBuf::from(first);
            if !(p.is_absolute() || first.contains('/') || cfg!(windows) && first.contains('\\')) {
                bail!("program requires an explicit path; PATH lookup and shell interpreter inference are unsupported");
            }
            (p.clone(), cwd.join(p), None, None, false)
        }
    };
    for p in [&declared, &requested, cwd] {
        model::validate_path(p)?;
    }
    Ok(Declaration {
        declared_path: declared,
        requested_path: requested,
        cwd: cwd.into(),
        requirement,
        scope,
        cleanup,
    })
}

#[derive(Default)]
pub(super) struct Cache {
    observations: BTreeMap<PathBuf, Evidence>,
    alias_count: usize,
}
impl Cache {
    pub fn observe(&mut self, path: &Path, c: &Control) -> Result<Option<Evidence>> {
        c.check()?;
        if let Some(e) = self.observations.get(path) {
            return Ok(Some(e.clone()));
        }
        // Selected resources get reserved capacity even when alias coverage fills.
        if self.observations.len() >= resource_impact::MAX_DECLARATIONS + MAX_RESOURCES {
            return Ok(None);
        }
        let e = resource_inspection::inspect_evidence(path);
        c.check()?;
        self.observations.insert(path.to_owned(), e.clone());
        Ok(Some(e))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn impact(
    config: &DevdConfig,
    state: &Path,
    service: &str,
    selection: &Selection,
    declaration: &Declaration,
    evidence: &Evidence,
    cache: &mut Cache,
    c: &Control,
) -> Result<Impact> {
    let field = match selection {
        Selection::Requires { index } => Some(resource_impact::Field::Requires { index: *index }),
        Selection::Paths { name } => Some(resource_impact::Field::Paths { name: name.clone() }),
        _ => None,
    };
    let view = if let Some(field) = field {
        let target = resource_impact::Target {
            selector: resource_impact::Selector {
                service: service.into(),
                field,
            },
            declared_path: declaration.declared_path.clone(),
            requested_path: declaration.requested_path.clone(),
            declared_scope: declaration.scope,
            inspection: evidence.inspection.clone(),
            risk_hints: vec![],
        };
        let view = resource_impact::analyze_with(config, state, &target, |p| {
            if let Some(e) = cache.observations.get(p) {
                return Some(e.inspection.clone());
            }
            if cache.alias_count == resource_impact::MAX_DECLARATIONS {
                return None;
            }
            cache.alias_count += 1;
            cache.observe(p, c).ok().flatten().map(|e| e.inspection)
        })?;
        c.check()?;
        Some(view)
    } else {
        None
    };
    let graph = DependencyGraph::from_config(config)?;
    let mut services: BTreeSet<String> = if let Some(v) = &view {
        v.direct_consumers
            .iter()
            .map(|s| s.service.clone())
            .chain(v.downstream.iter().map(|s| s.service.clone()))
            .collect()
    } else {
        BTreeSet::from([service.into()])
    };
    // Include the directly declared service even when alias observations are incomplete.
    services.insert(service.into());
    let mut queue: VecDeque<_> = services.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        c.check()?;
        for next in graph
            .dependents(&name)
            .context("impact service is absent")?
        {
            if services.insert(next.clone()) {
                queue.push_back(next.clone());
            }
        }
    }
    Ok(Impact {
        source: "current-effective-disk-config",
        complete: view.as_ref().is_none_or(|v| v.complete),
        services: services.into_iter().collect(),
        resource_view: view,
    })
}
