//! Observe conditions, not file contents or identity. No process control here.
use std::{convert::Infallible, future::pending, path::PathBuf, sync::Arc, time::Duration};

use crate::{
    config::{PathRequirement, ServiceConfig},
    logging::{LogCollector, LogLevel},
};

use super::{
    events::{EventData, EventRecorder, PathConditionEvidence},
    path_requirements::{evaluate, resolve_path, PathRequirementFailureKind},
};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Condition {
    // Startup already proved every requirement satisfied.
    published: Option<PathRequirementFailureKind>,
    candidate: Option<Option<PathRequirementFailureKind>>,
}

impl Condition {
    /// Publish only after two consecutive matching samples. Returning true for
    /// recovery is distinct from having no transition to report.
    fn observe(&mut self, failure: Option<PathRequirementFailureKind>) -> bool {
        if failure == self.published {
            self.candidate = None;
            return false;
        }
        if self.candidate == Some(failure) {
            self.published = failure;
            self.candidate = None;
            true
        } else {
            self.candidate = Some(failure);
            false
        }
    }
}

struct Paths {
    requirements: Vec<PathRequirement>,
    cwd: Option<PathBuf>,
}

/// Driven by the actor's select loop for exactly one process generation. The
/// blocking read owns no recorder or logger, so dropping this future prevents
/// publication even when an in-flight OS call cannot be interrupted.
pub(super) async fn monitor(
    config: &ServiceConfig,
    name: &str,
    generation: Option<u64>,
    restart_count: u32,
    events: &EventRecorder,
    logs: &LogCollector,
) -> Infallible {
    if !config.monitor_requires || config.requires.is_empty() {
        return pending().await;
    }
    let paths = Arc::new(Paths {
        requirements: config.requires.clone(),
        cwd: config.cwd.clone(),
    });
    let mut conditions: Vec<_> = paths
        .requirements
        .iter()
        .map(|_| Condition::default())
        .collect();
    loop {
        // Sleep after each completed sample: no catch-up bursts after slow I/O.
        tokio::time::sleep(SAMPLE_INTERVAL).await;
        let input = paths.clone();
        let sample = tokio::task::spawn_blocking(move || {
            input
                .requirements
                .iter()
                .map(|requirement| {
                    evaluate(requirement, input.cwd.as_deref())
                        .err()
                        .map(|failure| failure.kind)
                })
                .collect::<Vec<_>>()
        })
        .await;
        let failures = sample.unwrap_or_else(|_| {
            vec![Some(PathRequirementFailureKind::Inspection); conditions.len()]
        });
        for (index, (condition, failure)) in conditions.iter_mut().zip(failures).enumerate() {
            if !condition.observe(failure) {
                continue;
            }
            let requirement = &paths.requirements[index];
            let path = resolve_path(&requirement.path, paths.cwd.as_deref())
                .to_string_lossy()
                .into_owned();
            logs.record(
                name,
                restart_count,
                if failure.is_some() {
                    LogLevel::Warn
                } else {
                    LogLevel::Info
                },
                format!(
                    "path condition requires[{index}] at {path:?}: {} (observation only)",
                    failure.map_or("recovered", PathRequirementFailureKind::summary)
                ),
                false,
            );
            events.record(
                Some(name),
                generation,
                None,
                EventData::PathConditionChanged {
                    evidence: PathConditionEvidence {
                        requirement_index: index,
                        requirement: requirement.kind,
                        path,
                        failure,
                    },
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_condition_debounces_deduplicates_and_tracks_reason_changes() {
        use PathRequirementFailureKind::{Missing, NotReadable, WrongType};
        let mut condition = Condition::default();
        for (sample, changed) in [
            (None, false),
            (Some(Missing), false),
            (None, false), // transient disappearance does not publish
            (Some(Missing), false),
            (Some(Missing), true),
            (Some(Missing), false),
            (Some(WrongType), false),
            (Some(NotReadable), false), // changing reasons break the streak
            (Some(NotReadable), true),
            (None, false),
            (Some(NotReadable), false), // transient recovery does not publish
            (None, false),
            (None, true),
            (None, false),
        ] {
            assert_eq!(condition.observe(sample), changed, "sample {sample:?}");
        }
    }
}
