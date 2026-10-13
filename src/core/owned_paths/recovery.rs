//! Recovery shares cleanup's directory capabilities, registration and shutdown lease.
use super::*;
use crate::core::{
    recovery::{self, Recipe, Report, StepResult, Versions},
    resource_inspection,
};
use chrono::Utc;

const RECEIPTS: &str = "recovery-receipts.json";
const MAX_RECEIPTS: usize = 64;

#[derive(Serialize)]
pub(crate) struct Plan {
    pub schema_version: u16,
    pub plan_id: String,
    pub run_id: String,
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub profile: Option<String>,
    pub service: String,
    pub recipe_digest: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub resources: Vec<Resource>,
    pub steps: Vec<StepPreview>,
    pub observation_atomic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_expired_worker_cannot_accept_or_rebuild_but_retains_unknown_receipt() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("devd.yml");
        let yaml = "version: '1'\nservices:\n  worker:\n    command: placeholder\n    paths:\n      CACHE: {scope: instance, path: cache, cleanup: true}\n";
        std::fs::write(&config_path, yaml).unwrap();
        let config = crate::config::ConfigLoader::from_str(yaml, &config_path).unwrap();
        let state_path = root.path().join("state/services.json");
        let state = state_path.parent().unwrap();
        let mut options = ManagerOptions::new(state_path.clone());
        options.config_path = Some(config_path.clone());
        let lease = Arc::new(StateStore::open(&state_path).await.unwrap());
        let run = prepare(&config, &options, "run", lease.clone())
            .await
            .unwrap()
            .unwrap();
        run.finish(lease).await.unwrap();
        std::fs::remove_dir_all(state.join("runtime/cache")).unwrap();
        let recipe: Recipe = serde_json::from_value(serde_json::json!({
            "version": 1, "service": "worker", "resources": ["CACHE"],
            "steps": [{"id": "build", "resource": "CACHE",
                "command": [std::env::current_exe().unwrap()], "timeout": "1s",
                "ensures": [{"type": "file", "path": "result"}]}]
        }))
        .unwrap();
        let versions = Versions {
            config: "config".into(),
            recipe: "recipe".into(),
            config_file: [0, 0],
            recipe_file: [0, 0],
        };
        let mut session =
            Session::open(config, &config_path, state, None, recipe, versions).unwrap();
        let plan = session.preview(recovery::now_seconds().unwrap()).unwrap();
        session.set_operation_deadline(std::time::Instant::now());
        assert!(session.begin(&plan).is_err());
        assert!(!state.join(RECEIPTS).exists());
        assert!(
            read_json::<Manifest>(&session.state, JOURNAL)
                .unwrap()
                .quiescent
        );
        session
            .set_operation_deadline(std::time::Instant::now() + std::time::Duration::from_secs(30));
        session.begin(&plan).unwrap();
        session.set_operation_deadline(std::time::Instant::now());
        assert!(session.rebuild(|| Ok(())).is_err());
        assert!(!state.join("runtime/cache").exists());
        assert!(StateStore::open(&state_path).await.is_err());
        let report = session.finish("unknown", None, false).unwrap();
        assert_eq!(report.outcome, "unknown");
        assert!(
            !read_json::<Manifest>(&session.state, JOURNAL)
                .unwrap()
                .quiescent
        );
    }
}
#[derive(Serialize)]
pub(crate) struct StepPreview {
    id: String,
    resource: String,
    program: PathBuf,
    argument_count: usize,
    timeout: String,
    requires: Vec<crate::config::PathRequirement>,
    ensures: Vec<crate::config::PathRequirement>,
}

pub(crate) struct Session {
    state: Dir,
    canonical: PathBuf,
    _lock: crate::platform::files::FileLock,
    lock_identity: FileId,
    manifest: Manifest,
    config: DevdConfig,
    recipe: Recipe,
    selected: Vec<PathBuf>,
    versions: Versions,
    receipts: Vec<Report>,
    active: Option<usize>,
    checkpoint: String,
    programs_fingerprint: String,
    expires_at: u64,
    operation_deadline: Option<std::time::Instant>,
}

impl Session {
    pub fn open(
        config: DevdConfig,
        config_path: &Path,
        state_path: &Path,
        profile: Option<&str>,
        recipe: Recipe,
        versions: Versions,
    ) -> Result<Self> {
        recipe.validate()?;
        let service = config
            .services
            .get(&recipe.service)
            .context("unknown recipe service")?;
        let mut selected = Vec::new();
        for name in &recipe.resources {
            let binding = service.paths.get(name).context("unknown recipe resource")?;
            if !binding.cleanup || binding.scope != crate::config::PathScope::Instance {
                bail!("recovery requires explicitly disposable instance resources");
            }
            selected.push(binding.path.clone());
        }
        let canonical = std::fs::canonicalize(state_path)?;
        protect_shared(&config, &canonical)?;
        let state = Dir::open_ambient_dir(&canonical, cap_std::ambient_authority())?;
        let lock = regular_file(&state, Path::new("services.json.lock"))?;
        let lock_identity = identity(&lock.metadata()?)?;
        let lock = crate::platform::files::FileLock::acquire(lock.into_std(), false).context(
            "instance is active or state lock is busy; stop the whole stack before recovery",
        )?;
        let manifest: Manifest = read_json(&state, JOURNAL)
            .context("recovery requires previously registered resources")?;
        manifest.verify(config_path, &canonical, profile, &state)?;
        let specs = declarations(&config);
        for path in &selected {
            let owned = manifest
                .directories
                .get(path)
                .context("resource has no ownership registration; refusing adoption")?;
            if specs.get(path) != Some(&owned.spec) || owned.deleting {
                bail!("resource registration changed or cleanup is incomplete");
            }
        }
        let receipts: Vec<Report> = match state.symlink_metadata(RECEIPTS) {
            Ok(_) => read_json(&state, RECEIPTS)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        if receipts.len() > MAX_RECEIPTS
            || receipts
                .iter()
                .any(|r| r.schema_version != 1 || r.steps.len() > recovery::MAX_STEPS)
        {
            bail!("invalid or excessive recovery receipts");
        }
        Ok(Self {
            state,
            canonical,
            _lock: lock,
            lock_identity,
            manifest,
            config,
            recipe,
            selected,
            versions,
            receipts,
            active: None,
            checkpoint: String::new(),
            programs_fingerprint: String::new(),
            expires_at: 0,
            operation_deadline: None,
        })
    }

    pub fn previous(&self, requested: &str) -> Option<Report> {
        self.receipts.iter().find(|r| r.plan_id == requested && r.run_id == self.manifest.run_id && r.recipe_digest == self.versions.recipe).map(|r| {
            let mut report = r.clone();
            report.already_attempted = true;
            if report.finished_at.is_none() {
                report.outcome = "unknown".into();
                report.failure = Some("execution was interrupted; reconcile effects manually, never replay automatically".into());
                for step in &mut report.steps {
                    if step.state == "accepted" { step.state = "unknown".into(); }
                }
            }
            report
        })
    }

    fn verify_identity(&self) -> Result<()> {
        self.manifest.verify(
            &self.manifest.config,
            &self.canonical,
            self.manifest.profile.as_deref(),
            &self.state,
        )?;
        let current = Dir::open_ambient_dir(&self.canonical, cap_std::ambient_authority())?;
        if identity(&current.dir_metadata()?)? != self.manifest.state_identity
            || identity(&regular_file(&self.state, Path::new("services.json.lock"))?.metadata()?)?
                != self.lock_identity
            || read_json::<Manifest>(&self.state, JOURNAL)? != self.manifest
        {
            bail!("recovery ownership or state lock changed");
        }
        protect_shared(&self.config, &self.canonical)?;
        Ok(())
    }

    fn resources(&self) -> Result<Vec<Resource>> {
        self.verify_identity()?;
        self.selected
            .iter()
            .map(|path| {
                let owned = &self.manifest.directories[path];
                let dir = owned_directory(&self.state, owned, false)?;
                // Pin the existing parent even when the registered leaf was lost.
                let relative = Path::new("runtime").join(path);
                let parent = open_directory(&self.state, relative.parent().unwrap(), false)?;
                let mut hash = Sha256::new();
                hash.update(serde_json::to_vec(&identity(&parent.dir_metadata()?)?)?);
                let (mut entries, mut bytes) = (0, 0);
                if let Some(dir) = &dir {
                    hash.update(serde_json::to_vec(&identity(&dir.dir_metadata()?)?)?);
                    scan(
                        dir,
                        0,
                        &mut entries,
                        &mut bytes,
                        &mut hash,
                        owned.directory.volume,
                    )?;
                }
                Ok(Resource {
                    service: owned.spec.service.clone(),
                    name: owned.spec.name.clone(),
                    path: self.canonical.join(relative),
                    present: dir.is_some(),
                    entries,
                    bytes,
                    fingerprint: format!("sha256:{:x}", hash.finalize()),
                })
            })
            .collect()
    }

    pub fn program(&self, index: usize) -> PathBuf {
        self.config.services[&self.recipe.service]
            .cwd
            .as_deref()
            .unwrap_or(Path::new("."))
            .join(&self.recipe.steps[index].command[0])
    }
    pub fn root(&self, index: usize) -> PathBuf {
        let name = &self.recipe.steps[index].resource;
        self.canonical
            .join("runtime")
            .join(&self.config.services[&self.recipe.service].paths[name].path)
    }
    fn programs(&self) -> Result<Vec<(resource_inspection::Evidence, FileId)>> {
        (0..self.recipe.steps.len())
            .map(|index| {
                let evidence = resource_inspection::inspect_evidence(&self.program(index));
                if !evidence.inspection.complete
                    || evidence.inspection.change_detected
                    || evidence.inspection.failure.is_some()
                    || evidence
                        .inspection
                        .nodes
                        .last()
                        .is_none_or(|n| n.kind != resource_inspection::NodeKind::File)
                {
                    bail!("recipe program is unavailable or cannot be observed reliably");
                }
                // Diagnostic metadata lacks Windows file identity. Pin the resolved
                // regular file with a handle on both platforms, without reading it.
                let resolved = Path::new(
                    evidence
                        .inspection
                        .resolved
                        .as_deref()
                        .context("unresolved recipe program")?,
                );
                let parent = Dir::open_ambient_dir(
                    resolved.parent().context("invalid recipe program path")?,
                    cap_std::ambient_authority(),
                )?;
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
                #[cfg(windows)]
                options.custom_flags(
                    windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
                );
                let file = parent.open_with(
                    resolved.file_name().context("invalid program name")?,
                    &options,
                )?;
                let metadata = file.metadata()?;
                if !metadata.is_file()
                    || is_link(&metadata)
                    || digest(&resource_inspection::inspect_evidence(&self.program(index)))?
                        != digest(&evidence)?
                {
                    bail!("recipe program changed while opening its handle");
                }
                Ok((evidence, identity(&metadata)?))
            })
            .collect()
    }
    fn snapshot(&self) -> Result<String> {
        digest(&(self.resources()?, self.programs()?))
    }

    pub fn preview(&mut self, issued_at: u64) -> Result<Plan> {
        let now = recovery::now_seconds()?;
        if issued_at > now || now.saturating_sub(issued_at) >= recovery::PLAN_LIFETIME {
            bail!("recovery plan expired; preview again");
        }
        if !self.manifest.quiescent {
            bail!("last owned run did not finish safely; recovery cannot trust historical PIDs or replay unknown commands");
        }
        if self
            .receipts
            .iter()
            .any(|r| r.run_id == self.manifest.run_id && r.recipe_digest == self.versions.recipe)
        {
            bail!("this recipe was already attempted in this run; inspect its receipt, reconcile effects and explicitly revise the recipe or start a new run");
        }
        if self.receipts.len() >= MAX_RECEIPTS {
            bail!("recovery receipt limit reached (64); archive and reconcile receipts manually");
        }
        let resources = self.resources()?;
        let programs = self.programs()?;
        self.programs_fingerprint = digest(&programs)?;
        self.expires_at = issued_at + recovery::PLAN_LIFETIME;
        self.checkpoint = digest(&(&resources, &programs))?;
        let hash = digest(&(
            &self.manifest,
            &self.lock_identity,
            &self.versions,
            &self.recipe,
            &self.checkpoint,
            issued_at,
        ))?;
        let steps = self
            .recipe
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| StepPreview {
                id: s.id.clone(),
                resource: s.resource.clone(),
                program: self.program(i),
                argument_count: s.command.len() - 1,
                timeout: s.timeout.clone(),
                requires: s.requires.clone(),
                ensures: s.ensures.clone(),
            })
            .collect();
        Ok(Plan {
            schema_version: 1,
            plan_id: format!("{hash}:{issued_at}"),
            run_id: self.manifest.run_id.clone(),
            config: self.manifest.config.clone(),
            state_dir: self.canonical.clone(),
            profile: self.manifest.profile.clone(),
            service: self.recipe.service.clone(),
            recipe_digest: self.versions.recipe.clone(),
            issued_at,
            expires_at: issued_at + recovery::PLAN_LIFETIME,
            resources,
            steps,
            observation_atomic: false,
        })
    }

    fn save_receipts(&self) -> Result<()> {
        write_json(&self.state, RECEIPTS, &self.receipts)
    }
    pub fn set_operation_deadline(&mut self, deadline: std::time::Instant) {
        self.operation_deadline = Some(deadline);
    }
    fn check_operation_deadline(&self) -> Result<()> {
        if self
            .operation_deadline
            .is_some_and(|d| std::time::Instant::now() >= d)
        {
            bail!("recovery file operation expired before side effects");
        }
        Ok(())
    }
    pub fn begin(&mut self, plan: &Plan) -> Result<()> {
        self.revalidate()?;
        self.receipts.push(Report {
            schema_version: 1,
            plan_id: plan.plan_id.clone(),
            run_id: self.manifest.run_id.clone(),
            recipe_digest: self.versions.recipe.clone(),
            outcome: "accepted".into(),
            started_at: Utc::now(),
            finished_at: None,
            steps: Vec::new(),
            rebuilt: Vec::new(),
            failure: None,
            already_attempted: false,
        });
        self.active = Some(self.receipts.len() - 1);
        self.save_receipts()?;
        // An interrupted command must never leave a shutdown receipt that permits start/clean.
        self.manifest.quiescent = false;
        self.manifest.save(&self.state)?;
        Ok(())
    }
    pub fn revalidate(&self) -> Result<()> {
        if recovery::now_seconds()? >= self.expires_at {
            bail!("recovery plan expired before the next operation");
        }
        if digest(&self.programs()?)? != self.programs_fingerprint {
            bail!("recipe program changed during recovery");
        }
        if self.snapshot()? != self.checkpoint {
            bail!("recovery resources or programs changed; preview again");
        }
        self.check_operation_deadline()
    }
    pub fn rebuild(&mut self, check: impl Fn() -> Result<()>) -> Result<()> {
        for path in self.selected.clone() {
            check()?;
            self.revalidate()?;
            if owned_directory(&self.state, &self.manifest.directories[&path], false)?.is_some() {
                continue;
            }
            let relative = Path::new("runtime").join(&path);
            let parent = open_directory(&self.state, relative.parent().unwrap(), false)?;
            let name = relative.file_name().unwrap();
            check()?;
            parent
                .create_dir(name)
                .context("refusing to adopt a replacement resource")?;
            let dir = open_directory(&parent, Path::new(name), false)?;
            let token = uuid::Uuid::new_v4().to_string();
            write_json(&dir, MARKER, &token)?;
            let owned = self.manifest.directories.get_mut(&path).unwrap();
            owned.directory = identity(&dir.dir_metadata()?)?;
            owned.token = token;
            self.manifest.save(&self.state)?;
            self.receipts[self.active.unwrap()]
                .rebuilt
                .push(self.canonical.join(relative));
            self.save_receipts()?;
            self.checkpoint = self.snapshot()?;
        }
        Ok(())
    }

    fn conditions(&self, index: usize, after: bool) -> Result<()> {
        let step = &self.recipe.steps[index];
        let path = &self.config.services[&self.recipe.service].paths[&step.resource].path;
        let dir = owned_directory(&self.state, &self.manifest.directories[path], false)?
            .context("recovery resource is missing")?;
        for condition in if after { &step.ensures } else { &step.requires } {
            let parent = condition.path.parent().unwrap_or(Path::new(""));
            let parent = open_directory(&dir, parent, false)?;
            let name = Path::new(
                condition
                    .path
                    .file_name()
                    .context("invalid condition path")?,
            );
            match condition.kind {
                crate::config::PathRequirementType::File => {
                    regular_file(&parent, name)?;
                }
                crate::config::PathRequirementType::Directory => {
                    open_directory(&parent, name, false)?;
                }
                _ => bail!("unsupported recovery condition"),
            }
        }
        Ok(())
    }
    pub fn accept_step(&mut self, index: usize) -> Result<bool> {
        self.revalidate()?;
        let valid = self.conditions(index, false).is_ok();
        self.check_operation_deadline()?;
        self.receipts[self.active.unwrap()].steps.push(StepResult {
            id: self.recipe.steps[index].id.clone(),
            state: if valid { "accepted" } else { "failed" }.into(),
            accepted_at: Utc::now(),
            finished_at: (!valid).then(Utc::now),
            exit_code: None,
            failure: (!valid).then(|| "step precondition failed; command was not spawned".into()),
        });
        self.save_receipts()?;
        Ok(valid)
    }
    pub fn before_spawn(&self, index: usize) -> Result<()> {
        self.revalidate()?;
        self.conditions(index, false)
            .context("step precondition changed before spawn")?;
        self.check_operation_deadline()
    }
    pub fn complete_step(
        &mut self,
        index: usize,
        state: &str,
        exit_code: Option<i32>,
    ) -> Result<bool> {
        let checked = self
            .verify_identity()
            .and_then(|()| {
                if digest(&self.programs()?)? != self.programs_fingerprint {
                    bail!("recipe program changed during recovery");
                }
                Ok(())
            })
            .and_then(|()| self.conditions(index, true))
            .and_then(|()| self.snapshot());
        let success = state == "completed" && checked.is_ok();
        let result = self.receipts[self.active.unwrap()]
            .steps
            .last_mut()
            .context("step was not accepted")?;
        result.state = if state == "completed" && !success {
            "failed"
        } else {
            state
        }
        .into();
        result.finished_at = Some(Utc::now());
        result.exit_code = exit_code;
        result.failure = (!success).then(|| {
            if checked.is_err() {
                "postcondition or resource verification failed"
            } else {
                "command did not complete successfully"
            }
            .into()
        });
        self.save_receipts()?;
        if let Ok(snapshot) = checked {
            self.checkpoint = snapshot;
        }
        Ok(success)
    }
    pub fn finish(&mut self, outcome: &str, failure: Option<String>, safe: bool) -> Result<Report> {
        self.verify_identity()?;
        let report = &mut self.receipts[self.active.context("recovery was not accepted")?];
        report.outcome = outcome.into();
        report.failure = failure;
        report.finished_at = Some(Utc::now());
        for step in &mut report.steps {
            if step.state == "accepted" {
                step.state = "unknown".into();
            }
        }
        self.save_receipts()?;
        // Unknown process cleanup cannot grant the next run permission to start.
        if safe {
            self.manifest.quiescent = true;
            self.manifest.save(&self.state)?;
        }
        Ok(self.receipts[self.active.unwrap()].clone())
    }
}
