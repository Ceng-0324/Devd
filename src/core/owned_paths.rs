//! Explicitly disposable directories. Paths alone never establish ownership.
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
#[cfg(windows)]
use cap_fs_ext::MetadataExt as _;
use cap_std::fs::{Dir, Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{service_manager::ManagerOptions, state_store::StateStore};
use crate::config::DevdConfig;

const JOURNAL: &str = "owned-paths.json";
const MARKER: &str = ".devd-owner.json";
const MAX_JSON: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_ROOTS: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    pub service: String,
    pub name: String,
    pub path: PathBuf,
}

pub(crate) fn declarations(config: &DevdConfig) -> BTreeMap<PathBuf, Spec> {
    config
        .services
        .iter()
        .flat_map(|(service, config)| {
            config
                .paths
                .iter()
                .filter(|(_, binding)| binding.cleanup)
                .map(move |(name, binding)| {
                    (
                        binding.path.clone(),
                        Spec {
                            service: service.clone(),
                            name: name.clone(),
                            path: binding.path.clone(),
                        },
                    )
                })
        })
        .collect()
}

// Resolve existing ancestors without creating paths. Shared aliases must not
// quietly turn an explicitly disposable subtree into shared application data.
fn location(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut prefix = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(prefix) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(prefix.file_name().context("cannot resolve path identity")?);
                prefix = prefix.parent().context("cannot resolve path identity")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut resolved = std::fs::canonicalize(prefix)?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    #[cfg(windows)]
    {
        resolved = PathBuf::from(resolved.to_string_lossy().to_lowercase());
    }
    Ok(resolved)
}

fn protect_shared(config: &DevdConfig, state: &Path) -> Result<()> {
    let specs = declarations(config);
    if specs.is_empty() {
        return Ok(());
    }
    let shared = config
        .services
        .values()
        .flat_map(|service| {
            service
                .paths
                .values()
                .filter(|binding| binding.scope == crate::config::PathScope::Shared)
                .map(|binding| {
                    service
                        .cwd
                        .as_deref()
                        .unwrap_or(Path::new("."))
                        .join(&binding.path)
                })
        })
        .map(|path| location(&path))
        .collect::<Result<Vec<_>>>()?;
    for spec in specs.values() {
        let owned = location(&state.join("runtime").join(&spec.path))?;
        if shared
            .iter()
            .any(|path| path.starts_with(&owned) || owned.starts_with(path))
        {
            bail!(
                "cleanup path overlaps a shared mapping: {}",
                spec.path.display()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FileId {
    volume: u64,
    index: u64,
    created: Option<(u64, u32)>,
}

fn identity(metadata: &Metadata) -> Result<FileId> {
    // Both platforms obtain this metadata from an opened file/directory handle,
    // which is required by cap-fs-ext's Windows identity methods.
    let (volume, index) = (metadata.dev(), metadata.ino());
    Ok(FileId {
        volume,
        index,
        created: metadata
            .created()
            .ok()
            .and_then(|t| timestamp(t.into_std())),
    })
}

fn timestamp(time: std::time::SystemTime) -> Option<(u64, u32)> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| (d.as_secs(), d.subsec_nanos()))
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
}

fn regular_file(dir: &Dir, name: &Path) -> Result<cap_std::fs::File> {
    let metadata = dir.symlink_metadata(name)?;
    if !metadata.is_file() || is_link(&metadata) {
        bail!("not a regular file: {}", name.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let file = dir.open_with(name, &options)?;
    let metadata = file.metadata()?;
    let single = metadata.nlink() == 1;
    if !metadata.is_file() || is_link(&metadata) || !single {
        bail!(
            "links and special files are not eligible: {}",
            name.display()
        );
    }
    Ok(file)
}

fn read_json<T: serde::de::DeserializeOwned>(dir: &Dir, name: &str) -> Result<T> {
    let mut bytes = Vec::new();
    regular_file(dir, Path::new(name))?
        .take(MAX_JSON + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JSON {
        bail!("ownership record exceeds size limit");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json(dir: &Dir, name: &str, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() as u64 > MAX_JSON {
        bail!("ownership record exceeds size limit");
    }
    if dir.symlink_metadata(name).is_ok() {
        regular_file(dir, Path::new(name))?;
    }
    let temporary = format!(".owned-{}.tmp", uuid::Uuid::new_v4());
    let result = (|| -> Result<()> {
        let mut file =
            dir.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temporary, dir, name)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = dir.remove_file(&temporary);
    }
    result
}

/// Reject each redirected component; all subsequent operations remain relative
/// to these handles even if an ambient parent is renamed.
fn open_directory(base: &Dir, path: &Path, create: bool) -> Result<Dir> {
    let mut current = base.try_clone()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            bail!("invalid owned relative path");
        };
        if create {
            match current.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let metadata = current.symlink_metadata(name)?;
        if !metadata.is_dir() || is_link(&metadata) {
            bail!("owned path component is a link or not a directory");
        }
        current = current.open_dir(name)?;
    }
    Ok(current)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Owned {
    spec: Spec,
    directory: FileId,
    token: String,
    /// Written before deletion, so an interrupted operation may have removed
    /// the marker. The original directory identity is still mandatory.
    deleting: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u16,
    config: PathBuf,
    state_dir: PathBuf,
    profile: Option<String>,
    state_identity: FileId,
    revision: String,
    run_id: String,
    quiescent: bool,
    directories: BTreeMap<PathBuf, Owned>,
    completed_plan: Option<String>,
    completed_config: Option<String>,
}

impl Manifest {
    fn verify(
        &self,
        config: &Path,
        state_path: &Path,
        profile: Option<&str>,
        state: &Dir,
    ) -> Result<()> {
        if self.schema_version != 1
            || self.config != config
            || self.state_dir != state_path
            || self.profile.as_deref() != profile
            || self.state_identity != identity(&state.dir_metadata()?)?
            || self.directories.len() > MAX_ROOTS
        {
            bail!("ownership identity changed; refusing moved or foreign resources");
        }
        for (path, owned) in &self.directories {
            if path != &owned.spec.path
                || path
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
            {
                bail!("invalid ownership path");
            }
        }
        Ok(())
    }

    fn save(&mut self, state: &Dir) -> Result<()> {
        self.revision = uuid::Uuid::new_v4().to_string();
        write_json(state, JOURNAL, self)
    }
}

fn owned_directory(state: &Dir, owned: &Owned, deleting: bool) -> Result<Option<Dir>> {
    let relative = Path::new("runtime").join(&owned.spec.path);
    match state.symlink_metadata(&relative) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_dir() || is_link(&metadata) => {
            bail!("owned directory path is a link or not a directory");
        }
        Ok(_) => {}
    }
    let dir = if deleting {
        let parent = open_directory(
            state,
            relative.parent().context("missing resource parent")?,
            false,
        )?;
        crate::platform::files::open_owned_directory(
            &parent,
            Path::new(relative.file_name().context("missing resource name")?),
        )?
    } else {
        open_directory(state, &relative, false)?
    };
    if identity(&dir.dir_metadata()?)? != owned.directory {
        bail!(
            "owned directory was replaced: {}",
            owned.spec.path.display()
        );
    }
    if !owned.deleting {
        let token: String =
            read_json(&dir, MARKER).context("missing or invalid directory ownership marker")?;
        if token != owned.token {
            bail!("directory ownership marker changed");
        }
    }
    Ok(Some(dir))
}

pub(crate) struct RunOwnership {
    state: Dir,
    manifest: Manifest,
}

pub(crate) async fn prepare(
    config: &DevdConfig,
    options: &ManagerOptions,
    run_id: &str,
    lease: Arc<StateStore>,
) -> Result<Option<RunOwnership>> {
    let specs = declarations(config);
    let configuration = config.clone();
    let options = options.clone();
    let run_id = run_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let _lease = lease;
        let state_path = options
            .state_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let state_path = std::fs::canonicalize(state_path)?;
        let state = Dir::open_ambient_dir(&state_path, cap_std::ambient_authority())?;
        let exists = match state.symlink_metadata(JOURNAL) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if specs.is_empty() && !exists {
            return Ok(None);
        }
        protect_shared(&configuration, &state_path)?;
        if specs.len() > MAX_ROOTS {
            bail!("at most {MAX_ROOTS} cleanup directories are supported");
        }
        let config = options
            .config_path
            .context("cleanup paths require a configuration identity")?;
        let mut manifest = if exists {
            let manifest: Manifest = read_json(&state, JOURNAL)?;
            manifest.verify(&config, &state_path, options.profile.as_deref(), &state)?;
            if !manifest.quiescent {
                bail!("previous owned run did not finish safely; manual reconciliation required (historical PIDs are never trusted)");
            }
            if manifest.directories.values().any(|owned| owned.deleting) {
                bail!("cleanup was interrupted; preview and finish cleanup before starting");
            }
            manifest
        } else {
            Manifest {
                schema_version: 1,
                config,
                state_dir: state_path,
                profile: options.profile,
                state_identity: identity(&state.dir_metadata()?)?,
                revision: String::new(),
                run_id: run_id.clone(),
                quiescent: true,
                directories: BTreeMap::new(),
                completed_plan: None,
                completed_config: None,
            }
        };
        let new_roots = specs
            .keys()
            .filter(|path| !manifest.directories.contains_key(*path))
            .count();
        if manifest.directories.len() + new_roots > MAX_ROOTS {
            bail!("at most {MAX_ROOTS} registered cleanup directories are supported, including retained records");
        }
        for (path, spec) in specs {
            if manifest.directories.keys().any(|other| {
                other != &path && (path.starts_with(other) || other.starts_with(&path))
            }) {
                bail!("cleanup declaration overlaps an existing ownership record; clean or retain that directory separately");
            }
            if let Some(owned) = manifest.directories.get(&path) {
                if owned.spec != spec {
                    bail!("owned directory cannot be reassigned to another binding");
                }
                if owned_directory(&state, owned, false)?.is_some() {
                    continue;
                }
            }
            let parent = Path::new("runtime").join(path.parent().unwrap_or(Path::new("")));
            let parent = open_directory(&state, &parent, true)?;
            let name = path.file_name().context("owned path has no name")?;
            // create_dir (not create_dir_all) is the ownership boundary.
            parent.create_dir(name).with_context(|| {
                format!("refusing to adopt existing unowned directory {}", path.display())
            })?;
            let dir = open_directory(&parent, Path::new(name), false)?;
            let token = uuid::Uuid::new_v4().to_string();
            write_json(&dir, MARKER, &token)?;
            manifest.directories.insert(path, Owned {
                spec,
                directory: identity(&dir.dir_metadata()?)?,
                token,
                deleting: false,
            });
            manifest.save(&state)?;
        }
        manifest.run_id = run_id;
        manifest.quiescent = false;
        manifest.completed_plan = None;
        manifest.completed_config = None;
        manifest.save(&state)?;
        Ok(Some(RunOwnership { state, manifest }))
    })
    .await
    .context("ownership preparation task failed")?
}

impl RunOwnership {
    pub(crate) async fn finish(mut self, lease: Arc<StateStore>) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let _lease = lease;
            let current: Manifest = read_json(&self.state, JOURNAL)?;
            if current != self.manifest {
                bail!("ownership record changed while running");
            }
            self.manifest.quiescent = true;
            self.manifest.save(&self.state)
        })
        .await
        .context("ownership finalization task failed")?
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Resource {
    pub service: String,
    pub name: String,
    pub path: PathBuf,
    pub present: bool,
    pub entries: usize,
    pub bytes: u64,
    pub fingerprint: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Plan {
    pub schema_version: u16,
    pub plan_id: String,
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub profile: Option<String>,
    pub run_id: String,
    pub resources: Vec<Resource>,
    pub retained: Vec<PathBuf>,
    pub apply_available: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub schema_version: u16,
    pub plan_id: String,
    pub outcome: &'static str,
    pub removed: Vec<PathBuf>,
    pub already_absent: Vec<PathBuf>,
    pub already_applied: bool,
    pub failure: Option<String>,
}

fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}

fn scan(
    dir: &Dir,
    depth: usize,
    count: &mut usize,
    bytes: &mut u64,
    hash: &mut Sha256,
    volume: u64,
) -> Result<()> {
    if depth > 64 {
        bail!("cleanup tree exceeds 64 directory levels");
    }
    let mut entries = dir
        .entries()?
        .take(MAX_ENTRIES + 1)
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        *count += 1;
        if *count > MAX_ENTRIES {
            bail!("cleanup tree exceeds {MAX_ENTRIES} entries");
        }
        let name = entry.file_name();
        if name == JOURNAL || name == "services.json.lock" || (depth > 0 && name == MARKER) {
            bail!("cleanup tree contains nested devd ownership/state markers");
        }
        let kind = dir.symlink_metadata(&name)?;
        if is_link(&kind) || (!kind.is_file() && !kind.is_dir()) {
            bail!(
                "cleanup tree contains a link or special file: {}",
                name.to_string_lossy()
            );
        }
        let child = if kind.is_dir() {
            Some(open_directory(dir, Path::new(&name), false)?)
        } else {
            None
        };
        let metadata = if let Some(child) = &child {
            child.dir_metadata()?
        } else {
            regular_file(dir, Path::new(&name))?.metadata()?
        };
        let id = identity(&metadata)?;
        if id.volume != volume {
            bail!("cleanup cannot cross a filesystem mount");
        }
        hash.update(serde_json::to_vec(&(
            name.to_str().context("cleanup requires UTF-8 names")?,
            &id,
            metadata.is_dir(),
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|t| timestamp(t.into_std())),
        ))?);
        if let Some(child) = child {
            scan(&child, depth + 1, count, bytes, hash, volume)?;
        } else {
            *bytes = bytes.saturating_add(metadata.len());
        }
        hash.update([0xff]);
    }
    Ok(())
}

pub(crate) enum Output {
    Preview(Plan),
    Applied(Report),
}

/// Hold the existing state lock for this entire blocking operation.
pub(crate) fn clean(
    config: &DevdConfig,
    config_path: &Path,
    config_digest: &str,
    state_path: &Path,
    profile: Option<&str>,
    requested: Option<&str>,
) -> Result<Output> {
    let canonical = std::fs::canonicalize(state_path)?;
    protect_shared(config, &canonical)?;
    let state = Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())?;
    let lock = regular_file(&state, Path::new("services.json.lock"))
        .context("missing or unsafe instance state lock")?;
    let lock_identity = identity(&lock.metadata()?)?;
    let _lock = crate::platform::files::FileLock::acquire(lock.into_std(), false)
        .context("instance is active or its state lock is busy; stop it before cleanup")?;
    let mut manifest: Manifest = read_json(&state, JOURNAL)
        .context("no valid ownership record; existing directories cannot be adopted")?;
    manifest.verify(config_path, &canonical, profile, &state)?;
    if !manifest.quiescent {
        bail!("last owned run did not finish safely; unreachable is not proof of shutdown (no PID will be signalled)");
    }
    if let Some(requested) = requested.filter(|id| {
        Some(*id) == manifest.completed_plan.as_deref()
            && Some(config_digest) == manifest.completed_config.as_deref()
    }) {
        return Ok(Output::Applied(Report {
            schema_version: 1,
            plan_id: requested.into(),
            outcome: "applied",
            removed: vec![],
            already_absent: vec![],
            already_applied: true,
            failure: None,
        }));
    }
    let specs = declarations(config);
    let mut plan = Plan {
        schema_version: 1,
        plan_id: String::new(),
        config: config_path.into(),
        state_dir: canonical.clone(),
        profile: profile.map(str::to_owned),
        run_id: manifest.run_id.clone(),
        resources: vec![],
        retained: vec![],
        apply_available: true,
    };
    let mut opened = Vec::new();
    for (path, owned) in &manifest.directories {
        if specs.get(path) != Some(&owned.spec) {
            plan.retained.push(canonical.join("runtime").join(path));
            continue;
        }
        let dir = owned_directory(&state, owned, requested.is_some())?;
        let mut count = 0;
        let mut bytes = 0;
        let mut hash = Sha256::new();
        if let Some(dir) = &dir {
            scan(
                dir,
                0,
                &mut count,
                &mut bytes,
                &mut hash,
                owned.directory.volume,
            )?;
        }
        plan.resources.push(Resource {
            service: owned.spec.service.clone(),
            name: owned.spec.name.clone(),
            path: canonical.join("runtime").join(path),
            present: dir.is_some(),
            entries: count,
            bytes,
            fingerprint: format!("sha256:{:x}", hash.finalize()),
        });
        opened.push((path.clone(), dir));
    }
    for (path, spec) in &specs {
        if !manifest.directories.contains_key(path) {
            plan.retained
                .push(canonical.join("runtime").join(&spec.path));
        }
    }
    plan.plan_id = digest(&(&plan, &manifest, config_digest, &lock_identity))?;
    let Some(requested) = requested else {
        return Ok(Output::Preview(plan));
    };
    if requested != plan.plan_id {
        bail!("cleanup plan is stale; run 'devd clean --dry-run' again");
    }
    let mut report = Report {
        schema_version: 1,
        plan_id: plan.plan_id,
        outcome: "applied",
        removed: vec![],
        already_absent: vec![],
        already_applied: false,
        failure: None,
    };
    for (path, dir) in opened {
        let result = (|| -> Result<()> {
            if identity(&regular_file(&state, Path::new("services.json.lock"))?.metadata()?)?
                != lock_identity
                || read_json::<Manifest>(&state, JOURNAL)? != manifest
            {
                bail!("ownership or state lock changed during cleanup");
            }
            manifest.directories.get_mut(&path).unwrap().deleting = true;
            manifest.save(&state)?; // Persist interruption state before any deletion.
            if let Some(dir) = dir {
                crate::platform::files::remove_owned_directory(dir)
                    .context("cannot remove owned directory; cleanup may be partial")?;
                report.removed.push(canonical.join("runtime").join(&path));
            } else {
                report
                    .already_absent
                    .push(canonical.join("runtime").join(&path));
            }
            manifest.directories.remove(&path);
            manifest.save(&state)?;
            Ok(())
        })();
        if let Err(error) = result {
            report.outcome = "partial";
            report.failure = Some(format!("{error:#}; preview again before retrying"));
            return Ok(Output::Applied(report));
        }
    }
    manifest.completed_plan = Some(requested.to_owned());
    manifest.completed_config = Some(config_digest.to_owned());
    if let Err(error) = manifest.save(&state) {
        report.outcome = "partial";
        report.failure = Some(format!(
            "directories removed but receipt could not be saved: {error:#}"
        ));
    }
    Ok(Output::Applied(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigLoader;

    struct Fixture {
        root: tempfile::TempDir,
        config: DevdConfig,
        options: ManagerOptions,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("devd.yml");
            let yaml = "version: '1'\nservices:\n  worker:\n    command: placeholder\n    paths:\n      CACHE: {scope: instance, path: cache, cleanup: true}\n      KEEP: {scope: instance, path: keep}\n      SHARED: {scope: shared, path: shared}\n";
            std::fs::write(&path, yaml).unwrap();
            let config = ConfigLoader::from_str(yaml, &path).unwrap();
            config.validate().unwrap();
            let mut options = ManagerOptions::new(root.path().join("state/services.json"));
            options.config_path = Some(path);
            Self {
                root,
                config,
                options,
            }
        }
        fn state(&self) -> &Path {
            self.options.state_path.parent().unwrap()
        }
        fn cache(&self) -> PathBuf {
            self.state().join("runtime/cache")
        }
        async fn stopped(&self) {
            let lease = Arc::new(StateStore::open(&self.options.state_path).await.unwrap());
            let run = prepare(
                &self.config,
                &self.options,
                &uuid::Uuid::new_v4().to_string(),
                lease.clone(),
            )
            .await
            .unwrap()
            .unwrap();
            run.finish(lease).await.unwrap();
        }
        fn clean(&self, plan: Option<&str>) -> Result<Output> {
            clean(
                &self.config,
                self.options.config_path.as_ref().unwrap(),
                "config-v1",
                self.state(),
                self.options.profile.as_deref(),
                plan,
            )
        }
        fn preview(&self) -> Plan {
            let Output::Preview(plan) = self.clean(None).unwrap() else {
                panic!()
            };
            plan
        }
    }

    #[tokio::test]
    async fn test_owned_cleanup_preserves_unregistered_data_and_rejects_stale_plans() {
        let fixture = Fixture::new();
        fixture.stopped().await;
        std::fs::write(fixture.cache().join("discard"), "cache").unwrap();
        for path in ["runtime/keep", "logs", "events", "snapshots"] {
            std::fs::create_dir_all(fixture.state().join(path)).unwrap();
            std::fs::write(fixture.state().join(path).join("keep"), "user data").unwrap();
        }
        let before = std::fs::read(fixture.state().join(JOURNAL)).unwrap();
        let plan = fixture.preview();
        assert_eq!(plan.resources.len(), 1);
        assert!(fixture.cache().join("discard").exists());
        assert_eq!(
            std::fs::read(fixture.state().join(JOURNAL)).unwrap(),
            before,
            "preview must not write the journal"
        );
        std::fs::write(fixture.cache().join("new"), "added after preview").unwrap();
        assert!(fixture
            .clean(Some(&plan.plan_id))
            .err()
            .unwrap()
            .to_string()
            .contains("stale"));
        assert!(fixture.cache().join("discard").exists());
        let plan = fixture.preview();
        let Output::Applied(report) = fixture.clean(Some(&plan.plan_id)).unwrap() else {
            panic!()
        };
        assert_eq!(report.outcome, "applied");
        assert!(!fixture.cache().exists());
        for path in ["runtime/keep", "logs", "events", "snapshots"] {
            assert_eq!(
                std::fs::read_to_string(fixture.state().join(path).join("keep")).unwrap(),
                "user data"
            );
        }
        std::fs::create_dir(fixture.cache()).unwrap();
        std::fs::write(fixture.cache().join("user"), "do not delete on retry").unwrap();
        let Output::Applied(report) = fixture.clean(Some(&plan.plan_id)).unwrap() else {
            panic!()
        };
        assert!(report.already_applied);
        assert!(fixture.cache().join("user").exists());
        assert!(fixture.preview().resources.is_empty());
    }

    #[tokio::test]
    async fn test_owned_paths_refuse_adoption_active_and_unclean_runs() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.cache()).unwrap();
        std::fs::write(fixture.cache().join("user"), "keep").unwrap();
        let lease = Arc::new(StateStore::open(&fixture.options.state_path).await.unwrap());
        let error = prepare(&fixture.config, &fixture.options, "run", lease.clone())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("refusing to adopt"));
        assert_eq!(
            std::fs::read_to_string(fixture.cache().join("user")).unwrap(),
            "keep"
        );
        let fixture = Fixture::new();
        let lease = Arc::new(StateStore::open(&fixture.options.state_path).await.unwrap());
        let run = prepare(&fixture.config, &fixture.options, "run", lease.clone())
            .await
            .unwrap();
        assert!(fixture
            .clean(None)
            .err()
            .unwrap()
            .to_string()
            .contains("active"));
        drop(run); // no successful manager completion
        drop(lease);
        assert!(fixture
            .clean(None)
            .err()
            .unwrap()
            .to_string()
            .contains("did not finish safely"));
        let lease = Arc::new(StateStore::open(&fixture.options.state_path).await.unwrap());
        assert!(prepare(&fixture.config, &fixture.options, "next", lease)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("manual reconciliation"));
        assert!(fixture.cache().exists());
    }

    #[tokio::test]
    async fn test_owned_cleanup_rejects_replacements_revocation_and_shared_aliases() {
        let mut fixture = Fixture::new();
        fixture.stopped().await;
        let plan = fixture.preview();
        fixture
            .config
            .services
            .get_mut("worker")
            .unwrap()
            .paths
            .get_mut("CACHE")
            .unwrap()
            .cleanup = false;
        assert!(fixture
            .clean(Some(&plan.plan_id))
            .err()
            .unwrap()
            .to_string()
            .contains("stale"));
        assert!(fixture.preview().resources.is_empty());
        fixture
            .config
            .services
            .get_mut("worker")
            .unwrap()
            .paths
            .get_mut("CACHE")
            .unwrap()
            .cleanup = true;
        fixture
            .config
            .services
            .get_mut("worker")
            .unwrap()
            .paths
            .get_mut("SHARED")
            .unwrap()
            .path = fixture.cache();
        assert!(fixture
            .clean(None)
            .err()
            .unwrap()
            .to_string()
            .contains("shared mapping"));
        fixture
            .config
            .services
            .get_mut("worker")
            .unwrap()
            .paths
            .get_mut("SHARED")
            .unwrap()
            .path = fixture.root.path().join("shared");
        std::fs::rename(fixture.cache(), fixture.root.path().join("moved-cache")).unwrap();
        std::fs::create_dir(fixture.cache()).unwrap();
        std::fs::write(fixture.cache().join("user"), "replacement").unwrap();
        assert!(fixture
            .clean(Some(&plan.plan_id))
            .err()
            .unwrap()
            .to_string()
            .contains("replaced"));
        assert!(fixture.cache().join("user").exists());
        assert!(fixture.root.path().join("moved-cache").exists());
    }

    #[tokio::test]
    async fn test_owned_cleanup_recovers_interrupted_deletion_with_a_fresh_plan() {
        let fixture = Fixture::new();
        fixture.stopped().await;
        std::fs::write(fixture.cache().join("remaining"), "partial cleanup").unwrap();
        let previous = fixture.preview();
        let state = Dir::open_ambient_dir(fixture.state(), cap_std::ambient_authority()).unwrap();
        let mut manifest: Manifest = read_json(&state, JOURNAL).unwrap();
        manifest
            .directories
            .get_mut(Path::new("cache"))
            .unwrap()
            .deleting = true;
        manifest.save(&state).unwrap();
        std::fs::remove_file(fixture.cache().join(MARKER)).unwrap();
        assert!(fixture
            .clean(Some(&previous.plan_id))
            .err()
            .unwrap()
            .to_string()
            .contains("stale"));
        let lease = Arc::new(StateStore::open(&fixture.options.state_path).await.unwrap());
        assert!(prepare(&fixture.config, &fixture.options, "next", lease)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("interrupted"));
        let current = fixture.preview();
        let Output::Applied(report) = fixture.clean(Some(&current.plan_id)).unwrap() else {
            panic!()
        };
        assert_eq!(report.outcome, "applied");
        assert!(!fixture.cache().exists());
    }

    #[tokio::test]
    async fn test_owned_cleanup_rejects_hardlinks_and_moved_state() {
        let fixture = Fixture::new();
        fixture.stopped().await;
        let protected = fixture.root.path().join("outside");
        std::fs::write(&protected, "preserve").unwrap();
        std::fs::hard_link(&protected, fixture.cache().join("linked")).unwrap();
        assert!(fixture.clean(None).is_err());
        assert_eq!(std::fs::read_to_string(&protected).unwrap(), "preserve");
        std::fs::remove_file(fixture.cache().join("linked")).unwrap();
        let moved = fixture.root.path().join("moved-state");
        std::fs::rename(fixture.state(), &moved).unwrap();
        assert!(clean(
            &fixture.config,
            fixture.options.config_path.as_ref().unwrap(),
            "config-v1",
            &moved,
            None,
            None
        )
        .err()
        .unwrap()
        .to_string()
        .contains("identity changed"));
        assert!(moved.join("runtime/cache").exists());
    }

    #[tokio::test]
    async fn test_owned_cleanup_preflights_all_roots_and_rejects_nested_state() {
        let mut fixture = Fixture::new();
        fixture
            .config
            .services
            .get_mut("worker")
            .unwrap()
            .paths
            .get_mut("KEEP")
            .unwrap()
            .cleanup = true;
        fixture.stopped().await;
        let keep = fixture.state().join("runtime/keep");
        std::fs::create_dir(keep.join("nested")).unwrap();
        let plan = fixture.preview();
        for marker in [JOURNAL, MARKER, "services.json.lock"] {
            let path = keep.join("nested").join(marker);
            std::fs::write(&path, "another instance").unwrap();
            let error = fixture.clean(Some(&plan.plan_id)).err().unwrap();
            assert!(error.to_string().contains("nested devd"), "{error:#}");
            assert!(
                fixture.cache().join(MARKER).exists(),
                "no root may be removed before all roots pass"
            );
            std::fs::remove_file(path).unwrap();
        }
        let marker = std::fs::read(keep.join(MARKER)).unwrap();
        std::fs::write(keep.join(MARKER), "\"different-owner\"").unwrap();
        assert!(fixture
            .clean(None)
            .err()
            .unwrap()
            .to_string()
            .contains("marker changed"));
        assert!(fixture.cache().exists());
        std::fs::write(keep.join(MARKER), marker).unwrap();
        // A disappeared directory retires only its ownership record.
        std::fs::remove_dir_all(fixture.cache()).unwrap();
        let plan = fixture.preview();
        assert!(!plan.resources[0].present);
        let Output::Applied(report) = fixture.clean(Some(&plan.plan_id)).unwrap() else {
            panic!()
        };
        assert_eq!(report.outcome, "applied");
        assert_eq!(report.already_absent.len(), 1);
        assert_eq!(report.removed.len(), 1);
        assert!(!keep.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_owned_cleanup_refuses_redirected_root_and_ancestors() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.stopped().await;
        let original = fixture.root.path().join("original");
        std::fs::rename(fixture.cache(), &original).unwrap();
        symlink(&original, fixture.cache()).unwrap();
        assert!(fixture
            .clean(None)
            .err()
            .unwrap()
            .to_string()
            .contains("link"));
        assert!(original.join(MARKER).exists());
        std::fs::remove_file(fixture.cache()).unwrap();
        std::fs::rename(&original, fixture.cache()).unwrap();
        let runtime = fixture.state().join("runtime");
        std::fs::rename(&runtime, &original).unwrap();
        symlink(&original, &runtime).unwrap();
        assert!(fixture.clean(None).is_err());
        assert!(original.join("cache").join(MARKER).exists());
    }
}
