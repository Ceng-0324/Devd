//! Allowlisted capture-time facts. Configuration and path observations are not
//! an atomic trigger-time snapshot and never supply a temporal root-cause claim.
use super::super::{export::SafeServiceState, instances::Identity};
use super::{EndReason, Statistics};
use crate::{
    config::DevdConfig,
    core::{
        events::{EventData, LifecycleEvent, RestartSettings},
        resource_impact::{self, Field, Selector, Target},
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Begin {
    pub schema_version: u16,
    pub id: String,
    pub identity: Identity,
    pub trigger: LifecycleEvent,
    pub observed_at: DateTime<Utc>,
    pub observed_generation: Option<u64>,
    pub state: Option<SafeServiceState>,
    pub prior: Vec<LifecycleEvent>,
    pub prior_omitted: u64,
    pub statistics: Statistics,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct End {
    pub id: String,
    pub reason: EndReason,
    pub observed_at: DateTime<Utc>,
    pub following: Vec<LifecycleEvent>,
    pub following_omitted: Option<u64>,
    pub merged_triggers: Option<u64>,
    pub skipped_events: Option<u64>,
    pub statistics: Option<Statistics>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Configuration {
    pub(in crate::cli) source: String,
    pub(in crate::cli) atomic: bool,
    pub(in crate::cli) trigger_time_equivalence: String,
    pub(in crate::cli) services: Vec<Service>,
    pub(in crate::cli) omitted_services: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Service {
    pub(in crate::cli) name: String,
    pub(in crate::cli) dependencies: Vec<crate::config::Dependency>,
    pub(in crate::cli) omitted_dependencies: usize,
    pub(in crate::cli) healthcheck_configured: bool,
    pub(in crate::cli) monitor_requires: bool,
    pub(in crate::cli) restart: RestartSettings,
    pub(in crate::cli) restart_on_dep_recovery: bool,
    pub(in crate::cli) limits: Option<Limits>,
    pub(in crate::cli) requires_count: usize,
    pub(in crate::cli) paths_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Limits {
    pub(in crate::cli) cpu_percent: Option<u32>,
    pub(in crate::cli) memory_bytes: Option<u64>,
    pub(in crate::cli) on_exceed: crate::config::ResourceLimitAction,
}

pub(in crate::cli) fn configuration(config: &DevdConfig) -> Configuration {
    let mut summary = Configuration {
        source: "effective-runtime-config-observed-after-trigger".into(),
        atomic: false,
        trigger_time_equivalence: "not-guaranteed".into(),
        services: vec![],
        omitted_services: config.services.len(),
    };
    // Bound work as well as output; large configurations are explicitly partial.
    let mut services = BTreeMap::new();
    for (name, service) in &config.services {
        services.insert(name, service);
        if services.len() > 256 {
            services.pop_last();
        }
    }
    let mut bytes = 0;
    for (name, config) in services {
        if name.len() > 4096 {
            continue;
        }
        let dependencies: Vec<_> = config
            .depends_on
            .iter()
            .take(64)
            .filter(|d| d.service.len() <= 4096)
            .cloned()
            .collect();
        let service = Service {
            name: name.clone(),
            omitted_dependencies: config.depends_on.len() - dependencies.len(),
            dependencies,
            healthcheck_configured: config.healthcheck.is_some(),
            monitor_requires: config.monitor_requires,
            restart: (&config.restart).into(),
            restart_on_dep_recovery: config.restart_on_dep_recovery,
            limits: config
                .limits
                .as_ref()
                .and_then(|limits| limits.thresholds())
                .map(|limits| Limits {
                    cpu_percent: limits.cpu_percent,
                    memory_bytes: limits.memory_bytes,
                    on_exceed: limits.on_exceed,
                }),
            requires_count: config.requires.len(),
            paths_count: config.paths.len(),
        };
        let size = serde_json::to_vec(&service)
            .expect("allowlisted config")
            .len();
        if bytes + size > 128 * 1024 {
            break;
        }
        bytes += size;
        summary.services.push(service);
        summary.omitted_services -= 1;
    }
    summary
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Resources {
    pub id: String,
    pub(in crate::cli) source: String,
    pub(in crate::cli) observed_from: DateTime<Utc>,
    pub(in crate::cli) observed_to: DateTime<Utc>,
    pub(in crate::cli) atomic: bool,
    pub(in crate::cli) trigger_time_equivalence: String,
    pub observations: Vec<Resource>,
    pub omitted: usize,
    pub interrupted: bool,
    pub unavailable: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(in crate::cli) struct Resource {
    #[serde(flatten)]
    pub(in crate::cli) target: Target,
    pub(in crate::cli) requirement_type: Option<crate::config::PathRequirementType>,
    pub(in crate::cli) declared_cleanup: Option<bool>,
}

pub(in crate::cli) fn resources(
    capture: &Begin,
    config: &DevdConfig,
    state: &Path,
    deadline: Instant,
    finished: &AtomicBool,
    cancelled: &AtomicBool,
) -> Resources {
    let mut report = Resources {
        id: capture.id.clone(),
        source: "fresh-post-trigger-metadata-and-link-inspection".into(),
        observed_from: Utc::now(),
        observed_to: Utc::now(),
        atomic: false,
        trigger_time_equivalence: "not-guaranteed".into(),
        observations: vec![],
        omitted: 0,
        interrupted: false,
        unavailable: 0,
    };
    let Some(name) = capture.trigger.service.as_ref() else {
        return report;
    };
    let Some(service) = config.services.get(name) else {
        report.unavailable = 1;
        return report;
    };
    let mut fields = vec![];
    // Prioritize the condition actually mentioned by the trigger. The original
    // condition remains in the event even if the new inspection sees recovery.
    if let EventData::PathConditionChanged { evidence } = &capture.trigger.data {
        if evidence.requirement_index < service.requires.len() {
            fields.push(Field::Requires {
                index: evidence.requirement_index,
            });
        }
    }
    for index in 0..service.requires.len().min(8) {
        let field = Field::Requires { index };
        if fields.len() < 8 && !fields.contains(&field) {
            fields.push(field);
        }
    }
    for name in service.paths.keys().take(8 - fields.len()) {
        fields.push(Field::Paths { name: name.clone() });
    }
    let total = service.requires.len().saturating_add(service.paths.len());
    for field in fields {
        if Instant::now() >= deadline
            || finished.load(Ordering::Acquire)
            || cancelled.load(Ordering::Acquire)
        {
            report.interrupted = true;
            break;
        }
        let requirement_type = match &field {
            Field::Requires { index } => service.requires.get(*index).map(|r| r.kind),
            _ => None,
        };
        let declared_cleanup = match &field {
            Field::Paths { name } => service.paths.get(name).map(|p| p.cleanup),
            _ => None,
        };
        match resource_impact::select(
            config,
            state,
            Selector {
                service: name.clone(),
                field,
            },
        ) {
            Ok(target) => report.observations.push(Resource {
                target,
                requirement_type,
                declared_cleanup,
            }),
            Err(_) => report.unavailable += 1, // free-text OS/config errors are deliberately absent
        }
    }
    if Instant::now() >= deadline
        || finished.load(Ordering::Acquire)
        || cancelled.load(Ordering::Acquire)
    {
        report.interrupted = true;
    }
    report.omitted = total.saturating_sub(report.observations.len() + report.unavailable);
    report.observed_to = Utc::now();
    report
}

#[derive(Serialize)]
#[serde(tag = "record", content = "data", rename_all = "kebab-case")]
pub(in crate::cli) enum Record<'a> {
    Begin {
        capture: &'a Begin,
        configuration: Configuration,
        limitations: [&'static str; 5],
        retention_evicted_this_recorder: u64,
    },
    Resources(Resources),
    End(&'a End),
}

pub(in crate::cli) const LIMITATIONS: [&str; 5] = [
    "No application logs, full YAML, command/environment/dotenv contents or unreviewed error text.",
    "Paths and identifiers may be sensitive. No general-purpose redaction is promised.",
    "Prior/following events are historical context; temporal adjacency does not establish causation.",
    "Observed state/configuration may be newer than the trigger; PID values never authorize control.",
    "A missing end record or unfinished JSONL record is incomplete capture, never a successful completion.",
];
