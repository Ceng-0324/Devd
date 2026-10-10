//! Project-scoped discovery. Registry files are hints; only a live response proves identity.
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::protocol::{self, Request, Response};

const MAX_BYTES: u64 = 64 * 1024;
const MAX_RECORDS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Identity {
    pub schema_version: u16,
    pub instance_id: String,
    pub run_id: String,
    pub supervisor_pid: u32,
    pub started_at: DateTime<Utc>,
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub profile: Option<String>,
    pub project_root: PathBuf,
    pub git: Option<GitContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct GitContext {
    pub common_dir: PathBuf,
    /// Observed at supervisor startup, not continuously updated.
    pub branch_at_start: Option<String>,
    pub commit_at_start: Option<String>,
}

#[derive(Serialize)]
struct Entry {
    identity: Identity,
    status: &'static str,
}

#[derive(Serialize)]
struct Report {
    schema_version: u16,
    project_root: PathBuf,
    complete: bool,
    entries: Vec<Entry>,
    warnings: Vec<String>,
}

// Run only bounded, read-only Git commands. A cancelled command is killed.
async fn git(directory: &Path, arguments: &[&str]) -> Result<Option<String>> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(["-C"])
        .arg(directory)
        .args(arguments)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("missing git stdout")?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("Git output exceeds discovery limit");
        }
        if !child.wait().await?.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8(bytes).context("Git paths must be valid UTF-8")?,
        ))
    })
    .await
    .context("Git discovery timed out")?
}

async fn project(directory: &Path) -> Result<(PathBuf, Option<GitContext>)> {
    let Some(root) = git(directory, &["rev-parse", "--show-toplevel"]).await? else {
        return Ok((tokio::fs::canonicalize(directory).await?, None));
    };
    let root = tokio::fs::canonicalize(root.trim_end_matches(['\r', '\n'])).await?;
    let common = git(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?
    .context("cannot resolve Git repository identity")?;
    let branch = git(&root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let commit = git(&root, &["rev-parse", "--verify", "HEAD"]).await?;
    Ok((
        root,
        Some(GitContext {
            common_dir: tokio::fs::canonicalize(common.trim_end_matches(['\r', '\n'])).await?,
            branch_at_start: branch.map(|value| value.trim_end_matches(['\r', '\n']).to_owned()),
            commit_at_start: commit.map(|value| value.trim_end_matches(['\r', '\n']).to_owned()),
        }),
    ))
}

fn instance_id(config: &Path, state: &Path, profile: Option<&str>) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(config, state, profile))?)
    ))
}

/// Called under the supervisor state lock and before any service is spawned.
pub(super) async fn register(
    config: PathBuf,
    state: &Path,
    profile: Option<String>,
    run_id: String,
) -> Result<Identity> {
    let state_dir = tokio::fs::canonicalize(state).await?;
    let (project_root, git) =
        project(config.parent().context("missing configuration directory")?).await?;
    let identity = Identity {
        schema_version: 1,
        instance_id: instance_id(&config, &state_dir, profile.as_deref())?,
        run_id,
        supervisor_pid: std::process::id(),
        started_at: Utc::now(),
        config,
        state_dir,
        profile,
        project_root,
        git,
    };
    let record = identity.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let base = record.project_root.join(".devd");
        let index = base.join("instances");
        // Never follow a project index redirected through a symlink/reparse point.
        for directory in [&base, &index] {
            match std::fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            regular_directory(directory)?;
        }
        let path = index.join(format!("{}.json", record.instance_id));
        if path.exists() {
            crate::platform::files::open_regular(&path, false, true)?;
        }
        let bytes = serde_json::to_vec_pretty(&record)?;
        if bytes.len() as u64 > MAX_BYTES {
            bail!("instance identity exceeds size limit");
        }
        let mut file = tempfile::NamedTempFile::new_in(&index)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    })
    .await
    .context("instance registration task failed")??;
    Ok(identity)
}

fn regular_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || crate::platform::files::is_link(&metadata) {
        bail!(
            "instance index is not a regular directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn read_records(root: &Path) -> Result<(Vec<Identity>, Vec<String>)> {
    let base = root.join(".devd");
    let index = base.join("instances");
    if std::fs::symlink_metadata(&base)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok((vec![], vec![]));
    }
    regular_directory(&base)?;
    if std::fs::symlink_metadata(&index)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok((vec![], vec![]));
    }
    regular_directory(&index)?;
    let mut entries = std::fs::read_dir(index)?
        .take(MAX_RECORDS + 1)
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries.len() > MAX_RECORDS {
        bail!("project index exceeds {MAX_RECORDS} records");
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    for entry in entries {
        let path = entry.path();
        // Atomic publication leaves a temporary file visible only while writing.
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let read = || -> Result<Identity> {
            let file = crate::platform::files::open_regular(&path, false, true)?;
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_BYTES {
                bail!("oversized identity");
            }
            let record: Identity = serde_json::from_slice(&bytes)?;
            if record.schema_version != 1
                || record.project_root != root
                || !record.config.is_absolute()
                || !record.state_dir.is_absolute()
                || !record.config.starts_with(root)
                || uuid::Uuid::parse_str(&record.run_id).is_err()
                || record.instance_id
                    != instance_id(&record.config, &record.state_dir, record.profile.as_deref())?
                || path.file_name().and_then(|name| name.to_str())
                    != Some(&format!("{}.json", record.instance_id))
            {
                bail!("invalid instance identity");
            }
            Ok(record)
        };
        match read() {
            Ok(record) => records.push(record),
            Err(_) => warnings.push(format!(
                "cannot read valid instance record: {}",
                path.display()
            )),
        }
    }
    Ok((records, warnings))
}

pub(super) async fn show(socket: &Path, json: bool) -> Result<()> {
    let Response::Identity(identity) = protocol::request(socket, Request::Identity).await? else {
        bail!("supervisor does not support instance identity");
    };
    if json {
        super::output(&format!("{}\n", serde_json::to_string_pretty(&identity)?))
    } else {
        super::output(&format!("{}\n", render(&identity, "live")))
    }
}

fn render(identity: &Identity, status: &str) -> String {
    // Debug-escape user-controlled names/paths before terminal output.
    format!(
        "{}\t{}\trun={}\tproject={:?}\tconfig={:?}\tprofile={:?}\tbranch-at-start={:?}\tstate={:?}",
        identity.instance_id,
        status,
        identity.run_id,
        identity.project_root,
        identity.config,
        identity.profile,
        identity
            .git
            .as_ref()
            .and_then(|git| git.branch_at_start.as_deref()),
        identity.state_dir
    )
}

pub(super) async fn list(directory: &Path, json: bool) -> Result<()> {
    let (project_root, context) = project(directory).await?;
    let mut roots = BTreeSet::from([project_root.clone()]);
    let mut warnings = Vec::new();
    if context.is_some() {
        let output = git(&project_root, &["worktree", "list", "--porcelain", "-z"])
            .await?
            .context("cannot list Git worktrees")?;
        for field in output.split('\0') {
            if let Some(path) = field.strip_prefix("worktree ") {
                match tokio::fs::canonicalize(path).await {
                    Ok(path) => {
                        roots.insert(path);
                    }
                    Err(_) => warnings.push(format!("worktree unavailable: {path:?}")),
                }
            }
        }
    }
    if roots.len() > 64 {
        bail!("repository exceeds 64 worktrees");
    }
    let mut records = Vec::new();
    for root in roots {
        let label = root.clone();
        match tokio::task::spawn_blocking(move || read_records(&root)).await? {
            Ok((mut found, mut problems)) => {
                records.append(&mut found);
                warnings.append(&mut problems);
            }
            Err(_) => warnings.push(format!(
                "cannot inspect instance index in {}",
                label.display()
            )),
        }
        if records.len() > MAX_RECORDS {
            bail!("discovery exceeds {MAX_RECORDS} records");
        }
    }
    let mut entries = Vec::new();
    if let Some(context) = &context {
        records.retain(|record| {
            let matching = record
                .git
                .as_ref()
                .is_some_and(|git| git.common_dir == context.common_dir);
            if !matching {
                warnings.push(format!(
                    "instance {} belongs to a different repository context",
                    record.instance_id
                ));
            }
            matching
        });
    }
    // Bound both concurrency and per-endpoint time; silent endpoints cannot hang discovery.
    for batch in records.chunks(8) {
        let mut tasks = tokio::task::JoinSet::new();
        for identity in batch.iter().cloned() {
            tasks.spawn(inspect(identity));
        }
        while let Some(entry) = tasks.join_next().await {
            entries.push(entry?);
        }
    }
    entries.sort_by(|a, b| a.identity.instance_id.cmp(&b.identity.instance_id));
    let report = Report {
        schema_version: 1,
        project_root,
        complete: warnings.is_empty(),
        entries,
        warnings,
    };
    if json {
        super::output(&format!("{}\n", serde_json::to_string_pretty(&report)?))
    } else {
        let mut text = format!("Instances in {:?}\n", report.project_root);
        for entry in &report.entries {
            text.push_str(&format!("{}\n", render(&entry.identity, entry.status)));
        }
        if report.entries.is_empty() {
            text.push_str("No registered instances.\n");
        }
        for warning in &report.warnings {
            text.push_str(&format!("Warning: {warning:?}\n"));
        }
        super::output(&text)
    }
}

async fn inspect(identity: Identity) -> Entry {
    let result = tokio::time::timeout(
        Duration::from_millis(750),
        protocol::request(&identity.state_dir.join("control.sock"), Request::Identity),
    )
    .await;
    let status = match result {
        Ok(Ok(Response::Identity(live))) if *live == identity => "live",
        Ok(Ok(Response::Identity(_))) => "identity-mismatch",
        Ok(Ok(_)) => "unsupported",
        _ => "unreachable",
    };
    Entry { identity, status }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn fixture(root: &Path) -> Identity {
        let root = root.canonicalize().unwrap();
        let config = root.join("devd.yml");
        Identity {
            schema_version: 1,
            instance_id: instance_id(&config, &root, None).unwrap(),
            run_id: uuid::Uuid::new_v4().to_string(),
            supervisor_pid: 123,
            started_at: Utc::now(),
            config,
            state_dir: root.clone(),
            profile: None,
            project_root: root,
            git: None,
        }
    }

    #[test]
    fn test_record_identity_and_size_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let record = fixture(root.path());
        let index = record.project_root.join(".devd/instances");
        std::fs::create_dir_all(&index).unwrap();
        let path = index.join(format!("{}.json", record.instance_id));
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(
            read_records(&record.project_root).unwrap().0,
            std::slice::from_ref(&record)
        );
        for field in [
            "schema_version",
            "instance_id",
            "run_id",
            "project_root",
            "config",
            "state_dir",
        ] {
            let mut bad = serde_json::to_value(&record).unwrap();
            bad[field] = if field == "schema_version" {
                2.into()
            } else {
                "invalid".into()
            };
            std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
            let (records, warnings) = read_records(&record.project_root).unwrap();
            assert!(records.is_empty(), "{field}");
            assert_eq!(warnings.len(), 1);
        }
        std::fs::write(&path, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(read_records(&record.project_root).unwrap().1.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_registry_rejects_symlink_fifo_and_hardlink_without_reading_target() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".devd")).unwrap();
        symlink("missing", root.join(".devd/instances")).unwrap();
        assert!(read_records(&root).is_err());
        std::fs::remove_file(root.join(".devd/instances")).unwrap();
        let index = root.join(".devd/instances");
        std::fs::create_dir(&index).unwrap();
        std::fs::write(root.join("secret"), "secret").unwrap();
        symlink(root.join("secret"), index.join("link.json")).unwrap();
        std::fs::hard_link(root.join("secret"), index.join("hard.json")).unwrap();
        nix::unistd::mkfifo(&index.join("fifo.json"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let (records, warnings) = read_records(&root).unwrap();
        assert!(records.is_empty());
        assert_eq!(warnings.len(), 3);
        assert_eq!(
            std::fs::read_to_string(root.join("secret")).unwrap(),
            "secret"
        );
    }

    #[tokio::test]
    async fn test_discovery_bounds_silent_and_oversized_endpoint_responses() {
        for oversized in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let identity = fixture(root.path());
            let mut listener =
                super::super::transport::Listener::bind(&identity.state_dir.join("control.sock"))
                    .await
                    .unwrap();
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.unwrap();
                assert!(matches!(
                    protocol::read_request(&mut stream).await.unwrap(),
                    Request::Identity
                ));
                if oversized {
                    stream.write_u32(128 * 1024 + 1).await.unwrap();
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            let entry = tokio::time::timeout(Duration::from_secs(2), inspect(identity))
                .await
                .unwrap();
            assert_eq!(entry.status, "unreachable");
            server.abort();
            let _ = server.await;
        }
    }
}
