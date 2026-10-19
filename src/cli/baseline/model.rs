use super::*;
use crate::{
    config::{PathRequirementType, PathScope},
    core::resource_inspection::{self, Evidence, Failure, NodeKind},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{io::Write, str::FromStr};

pub(super) const MAX_RESOURCES: usize = 32;
pub(super) const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "field", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Selection {
    Requires { index: usize },
    Paths { name: String },
    Cwd,
    Program,
    EnvFile,
}
impl FromStr for Selection {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "cwd" => Ok(Self::Cwd),
            "program" => Ok(Self::Program),
            "env-file" => Ok(Self::EnvFile),
            _ if s.starts_with("requires:") => s[9..]
                .parse()
                .map(|index| Self::Requires { index })
                .map_err(|_| "requires expects a non-negative index".into()),
            _ if s.starts_with("paths:") && s.len() > 6 && s.len() <= 128 => Ok(Self::Paths {
                name: s[6..].into(),
            }),
            _ => Err("select requires:INDEX, paths:NAME, cwd, program or env-file".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Declaration {
    pub declared_path: PathBuf,
    pub requested_path: PathBuf,
    pub cwd: PathBuf,
    pub requirement: Option<PathRequirementType>,
    pub scope: Option<PathScope>,
    pub cleanup: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub selection: Selection,
    pub declaration: Declaration,
    pub evidence: Evidence,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextKey {
    pub config: PathBuf,
    pub state: PathBuf,
    pub profile: Option<String>,
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalScope {
    token: String,
    marker_identity: [u64; 2],
    platform: String,
    host_hint: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Baseline {
    pub schema_version: u16,
    pub source: String,
    pub id: String,
    pub context: ContextKey,
    pub local_scope: LocalScope,
    pub captured_from: DateTime<Utc>,
    pub captured_to: DateTime<Utc>,
    pub service: String,
    pub entries: Vec<Entry>,
}
impl Baseline {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported baseline schema {}", self.schema_version);
        }
        if self.source != "explicit-effective-disk-config"
            || self.captured_from > self.captured_to
            || self.service.is_empty()
            || self.service.len() > 256
            || self.entries.is_empty()
            || self.entries.len() > MAX_RESOURCES
        {
            bail!("invalid baseline header or resource count");
        }
        uuid::Uuid::parse_str(&self.id).context("invalid baseline ID")?;
        uuid::Uuid::parse_str(&self.local_scope.token).context("invalid baseline local scope")?;
        if self.local_scope.platform.len() > 64
            || self
                .local_scope
                .host_hint
                .as_ref()
                .is_some_and(|s| s.len() > 256)
        {
            bail!("invalid baseline local scope fields");
        }
        if let Some(profile) = &self.context.profile {
            crate::config::validate_profile_name(profile)?;
        }
        for p in [&self.context.config, &self.context.state] {
            validate_path(p)?;
        }
        let mut seen = BTreeSet::new();
        for e in &self.entries {
            if !seen.insert(&e.selection) {
                bail!("duplicate baseline selector");
            }
            if let Selection::Paths { name } = &e.selection
                && (name.is_empty() || name.len() > 128)
            {
                bail!("invalid path selector");
            }
            for p in [
                &e.declaration.cwd,
                &e.declaration.declared_path,
                &e.declaration.requested_path,
            ] {
                validate_path(p)?;
            }
            let i = &e.evidence.inspection;
            if i.atomic
                || i.nodes.len() > resource_inspection::MAX_NODES
                || i.nodes.len() != e.evidence.metadata.len()
                || i.failed_node.is_some_and(|n| n >= i.nodes.len())
            {
                bail!("invalid baseline inspection shape");
            }
            if i.complete
                && (i.change_detected
                    || matches!(
                        i.failure,
                        Some(
                            Failure::Unsupported
                                | Failure::NonUtf8
                                | Failure::Inspection
                                | Failure::Limit
                        )
                    ))
                || i.failure.is_some() && i.resolved.is_some()
                || i.failure.is_none()
                    && (i.nodes.is_empty()
                        || i.resolved.as_ref() != i.nodes.last().map(|n| &n.path)
                        || i.failed_node.is_some())
            {
                bail!("inconsistent baseline inspection outcome");
            }
            if let Some(p) = &i.resolved {
                validate_path(Path::new(p))?;
            }
            for (node, metadata) in i.nodes.iter().zip(&e.evidence.metadata) {
                validate_path(Path::new(&node.path))?;
                if node.target.is_some() && node.kind != NodeKind::Symlink
                    || metadata.is_none() && node.kind != NodeKind::Unknown
                    || i.failure.is_none()
                        && (node.kind == NodeKind::Unknown
                            || node.kind == NodeKind::Symlink && node.target.is_none())
                {
                    bail!("inconsistent baseline node evidence");
                }
                if let Some(p) = &node.target {
                    validate_path(Path::new(p))?;
                }
                if let Some(m) = metadata
                    && (m.identity.as_ref().is_some_and(|s| s.len() > 64)
                        || node.kind == NodeKind::Directory
                            && (m.size.is_some() || m.modified.is_some()))
                {
                    bail!("invalid baseline node metadata");
                }
            }
            if serde_json::to_vec(&i.nodes)?.len() > resource_inspection::MAX_CHAIN_BYTES {
                bail!("baseline path chain exceeds limit");
            }
            // No imported path is an IO source. Even valid evidence remains a recorded assertion.
        }
        Ok(())
    }
}
pub(super) fn validate_path(p: &Path) -> Result<()> {
    if p.to_str().is_none()
        || p.as_os_str().is_empty()
        || p.as_os_str().len() > resource_inspection::MAX_PATH_BYTES
        || p.to_str().is_some_and(|s| s.contains('\0'))
    {
        bail!("path is non-UTF-8, empty or exceeds inspection limit");
    }
    Ok(())
}

/// A retained local lineage marker plus its file identity. Copying the baseline
/// alone cannot make another directory comparable. This is not host authentication;
/// cloning the marker/context can defeat provenance, and no identity is portable.
pub(super) fn local_scope(state: &Path, create: bool, c: &Control) -> Result<Option<LocalScope>> {
    c.check()?;
    let marker = state.join("baseline-origin.id");
    if create {
        std::fs::create_dir_all(state)?;
        if !marker.try_exists()? {
            let mut temporary = tempfile::NamedTempFile::new_in(state)?;
            temporary.write_all(uuid::Uuid::new_v4().to_string().as_bytes())?;
            temporary.as_file().sync_all()?;
            c.check()?;
            match temporary.persist_noclobber(&marker) {
                Ok(_) => {}
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
        }
    }
    if !marker.try_exists()? {
        return Ok(None);
    }
    let read = output::read_regular(&marker, c, 64)?;
    let token = std::str::from_utf8(&read.bytes)?.to_owned();
    uuid::Uuid::parse_str(&token).context("invalid local baseline origin marker")?;
    c.check()?;
    Ok(Some(LocalScope {
        token,
        marker_identity: read.identity,
        platform: std::env::consts::OS.into(),
        host_hint: sysinfo::System::host_name(),
    }))
}

#[derive(Serialize)]
pub(super) struct Current {
    pub declaration: Declaration,
    pub evidence: Evidence,
}
#[derive(Serialize)]
pub(super) struct Impact {
    pub source: &'static str,
    pub complete: bool,
    pub services: Vec<String>,
    pub resource_view: Option<crate::core::resource_impact::View>,
}
#[derive(Serialize)]
pub(super) struct CheckEntry {
    pub recorded: Entry,
    pub selection: Selection,
    pub comparison: drift::Comparison,
    pub current: Option<Current>,
    pub current_declared_impact: Option<Impact>,
}
#[derive(Serialize)]
pub(super) struct CheckReport {
    pub schema_version: u16,
    pub source: &'static str,
    pub baseline_file: PathBuf,
    pub baseline_id: String,
    pub baseline_captured_from: DateTime<Utc>,
    pub baseline_captured_to: DateTime<Utc>,
    pub captured_from: DateTime<Utc>,
    pub captured_to: DateTime<Utc>,
    pub service: String,
    pub context: ContextKey,
    pub local_context_comparable: bool,
    pub outcome: drift::Outcome,
    pub entries: Vec<CheckEntry>,
    pub limitations: Vec<&'static str>,
}
