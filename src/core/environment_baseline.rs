//! Compare recorded observations; never opens paths or controls processes.
use super::resource_inspection::{Evidence, Failure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    NoObservedChange,
    Changed,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub outcome: Outcome,
    pub reasons: Vec<&'static str>,
    /// A changed observation may still have gaps; it is not a causal diagnosis.
    pub complete: bool,
}

pub fn compare(before: &Evidence, after: &Evidence) -> Comparison {
    let a = &before.inspection;
    let b = &after.inspection;
    let comparable = |e: &Evidence| {
        let i = &e.inspection;
        i.complete
            && !i.change_detected
            && !i.atomic
            && matches!(
                i.failure,
                None | Some(Failure::Missing | Failure::DanglingSymlink | Failure::WrongType)
            )
            && i.nodes.len() == e.metadata.len()
    };
    if !comparable(before) || !comparable(after) {
        return unknown("observation-incomplete-or-unsupported");
    }
    let mut reasons = vec![];
    if a.failure != b.failure {
        reasons.push("path-condition-changed");
    }
    if a.resolved != b.resolved {
        reasons.push("resolution-endpoint-changed");
    }
    if a.nodes
        .iter()
        .map(|n| (&n.path, n.kind, &n.target))
        .ne(b.nodes.iter().map(|n| (&n.path, n.kind, &n.target)))
    {
        reasons.push("path-chain-changed");
    }
    let mut metadata_changed = false;
    let mut unavailable = false;
    let coverage = before
        .metadata
        .iter()
        .chain(&after.metadata)
        .flatten()
        .all(|m| m.identity.is_some());
    // Compare only like-for-like nodes; a changed chain already proves structural drift.
    if reasons.is_empty() {
        for (a, b) in before.metadata.iter().zip(&after.metadata) {
            match (a, b) {
                (Some(a), Some(b)) => {
                    metadata_changed |= a.permissions != b.permissions;
                    macro_rules! compare_optional {
                        ($field:ident) => {
                            match (&a.$field, &b.$field) {
                                (Some(a), Some(b)) => metadata_changed |= a != b,
                                (None, None) => {}
                                _ => unavailable = true,
                            }
                        };
                    }
                    compare_optional!(identity);
                    compare_optional!(size);
                    compare_optional!(modified);
                    compare_optional!(created);
                }
                (None, None) => {}
                _ => unavailable = true,
            }
        }
    }
    if metadata_changed {
        reasons.push("node-metadata-changed");
    }
    let observed_change = !reasons.is_empty();
    if unavailable {
        reasons.push("metadata-comparison-unavailable");
    }
    let complete = coverage && !unavailable;

    Comparison {
        outcome: if observed_change {
            Outcome::Changed
        } else if unavailable {
            Outcome::Unknown
        } else {
            Outcome::NoObservedChange
        },
        reasons,
        complete,
    }
}

pub fn unknown(reason: &'static str) -> Comparison {
    Comparison {
        outcome: Outcome::Unknown,
        reasons: vec![reason],
        complete: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::resource_inspection;
    #[test]
    fn test_drift_missing_recovery_metadata_and_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let missing = resource_inspection::inspect_evidence(&path);
        assert_eq!(
            compare(&missing, &missing).outcome,
            Outcome::NoObservedChange
        );
        std::fs::write(&path, "secret").unwrap();
        let exists = resource_inspection::inspect_evidence(&path);
        assert_eq!(compare(&missing, &exists).outcome, Outcome::Changed);
        assert_eq!(compare(&exists, &exists).outcome, Outcome::NoObservedChange);
        std::fs::write(&path, "different length").unwrap();
        let changed = resource_inspection::inspect_evidence(&path);
        assert!(compare(&exists, &changed)
            .reasons
            .contains(&"node-metadata-changed"));
        let mut uncertain = changed.clone();
        uncertain.inspection.change_detected = true;
        assert_eq!(compare(&exists, &uncertain).outcome, Outcome::Unknown);
        uncertain = changed.clone();
        uncertain.inspection.failure = Some(Failure::Unsupported);
        assert_eq!(compare(&exists, &uncertain).outcome, Outcome::Unknown);
        let mut unavailable = exists.clone();
        unavailable
            .metadata
            .last_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .modified = None;
        assert_eq!(compare(&exists, &unavailable).outcome, Outcome::Unknown);
        uncertain = changed.clone();
        uncertain.metadata.clear();
        assert_eq!(compare(&exists, &uncertain).outcome, Outcome::Unknown);
    }
}
