//! Explicit recovery contracts. Commands are reviewed user code, not a sandbox.
use crate::config::{PathRequirement, PathRequirementType};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub(crate) const MAX_STEPS: usize = 32;
pub(crate) const PLAN_LIFETIME: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    pub version: u16,
    pub service: String,
    pub resources: Vec<String>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Step {
    pub id: String,
    pub resource: String,
    pub command: Vec<String>,
    pub timeout: String,
    #[serde(default)]
    pub requires: Vec<PathRequirement>,
    pub ensures: Vec<PathRequirement>,
}

impl Recipe {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.resources.is_empty()
            || self.resources.len() > 16
            || self.steps.is_empty()
            || self.steps.len() > MAX_STEPS
        {
            bail!("recipe requires version 1, 1–16 resources and 1–32 steps");
        }
        let resources: BTreeSet<_> = self.resources.iter().collect();
        if resources.len() != self.resources.len() {
            bail!("duplicate recovery resource");
        }
        let mut ids = BTreeSet::new();
        for step in &self.steps {
            if step.id.is_empty()
                || step.id.len() > 64
                || !step
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                || !ids.insert(&step.id)
            {
                bail!("step IDs must be unique ASCII identifiers (1–64 bytes)");
            }
            if !resources.contains(&step.resource) {
                bail!("step references an unselected resource");
            }
            if step.command.is_empty()
                || step.command.len() > 128
                || step.command[0].is_empty()
                || step
                    .command
                    .iter()
                    .any(|s| s.len() > 8192 || s.contains('\0'))
            {
                bail!("command requires 1–128 arguments of at most 8192 bytes");
            }
            let program = Path::new(&step.command[0]);
            if !program.is_absolute() && !step.command[0].contains(['/', '\\']) {
                bail!("recipe program must be an explicit path; PATH lookup is not supported");
            }
            step.duration()?;
            if step.requires.len() > 32 || step.ensures.is_empty() || step.ensures.len() > 32 {
                bail!("each step requires 1–32 ensures and at most 32 requires");
            }
            for condition in step.requires.iter().chain(&step.ensures) {
                validate_relative(&condition.path)?;
                if condition.kind == PathRequirementType::Symlink {
                    bail!("recovery conditions support only regular files and directories");
                }
            }
        }
        Ok(())
    }
}
impl Step {
    pub fn duration(&self) -> Result<Duration> {
        let duration = crate::config::parse_duration(&self.timeout)
            .map_err(anyhow::Error::msg)
            .context("invalid recipe timeout")?;
        if duration.is_zero() || duration > Duration::from_secs(300) {
            bail!("step timeout must be greater than zero and at most 5m");
        }
        Ok(duration)
    }
}
pub(crate) fn validate_relative(path: &Path) -> Result<()> {
    let text = path.to_str().context("recovery paths must be UTF-8")?;
    if text.is_empty()
        || text.len() > 4096
        || text.contains(['\\', ':', '\0'])
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with(".devd"))
    {
        bail!("recovery conditions must be plain relative paths without traversal or devd markers");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Versions {
    pub config: String,
    pub recipe: String,
    pub config_file: [u64; 2],
    pub recipe_file: [u64; 2],
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepResult {
    pub id: String,
    pub state: String,
    pub accepted_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
    pub failure: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub schema_version: u16,
    pub plan_id: String,
    pub run_id: String,
    pub recipe_digest: String,
    pub outcome: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub steps: Vec<StepResult>,
    pub rebuilt: Vec<PathBuf>,
    pub failure: Option<String>,
    pub already_attempted: bool,
}

pub(crate) fn now_seconds() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}
pub(crate) fn plan_time(plan: &str) -> Result<u64> {
    let (digest, time) = plan.rsplit_once(':').context("invalid recovery plan ID")?;
    let hex = digest
        .strip_prefix("sha256:")
        .context("invalid recovery plan ID")?;
    if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("invalid recovery plan ID");
    }
    time.parse().context("invalid recovery plan timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recipe() -> Recipe {
        serde_yaml::from_str("version: 1\nservice: worker\nresources: [CACHE]\nsteps:\n  - id: build\n    resource: CACHE\n    command: [/explicit/program, arg]\n    timeout: 1s\n    ensures: [{type: file, path: generated}]\n").unwrap()
    }
    #[test]
    fn test_recipe_contract_rejects_unsafe_and_excessive_inputs() {
        recipe().validate().unwrap();
        for path in [
            "../escape",
            "/absolute",
            "a/../escape",
            "a\\b",
            "C:stream",
            ".devd-owner.json",
            "a/.devd-marker",
        ] {
            let mut r = recipe();
            r.steps[0].ensures[0].path = path.into();
            assert!(r.validate().is_err(), "{path}");
        }
        for timeout in ["0s", "301s", "wrong"] {
            let mut r = recipe();
            r.steps[0].timeout = timeout.into();
            assert!(r.validate().is_err());
        }
        let mut r = recipe();
        r.steps[0].command = vec!["program".into()];
        assert!(r.validate().is_err());
        let mut r = recipe();
        r.steps[0].ensures.clear();
        assert!(r.validate().is_err());
        let mut r = recipe();
        r.steps[0].ensures[0].kind = PathRequirementType::Symlink;
        assert!(r.validate().is_err());
        let mut r = recipe();
        r.steps.push(r.steps[0].clone());
        assert!(r.validate().is_err());
        let mut r = recipe();
        r.resources.push("CACHE".into());
        assert!(r.validate().is_err());
        let mut r = recipe();
        r.steps[0].command.push("\0".into());
        assert!(r.validate().is_err());
        for id in ["", "sha256:x:0", "sha256:xyz:1", "sha256:000:1"] {
            assert!(plan_time(id).is_err());
        }
        assert!(serde_yaml::from_str::<Recipe>(
            "version: 1\nservice: x\nresources: []\nsteps: []\nextra: yes"
        )
        .is_err());
    }
}
