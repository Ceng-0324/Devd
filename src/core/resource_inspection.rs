//! Bounded, best-effort path observations. Never a startup or ownership decision.
use std::{
    collections::{HashSet, VecDeque},
    ffi::OsString,
    fs::{self, Metadata},
    io,
    path::{Component, Path, PathBuf},
};

use serde::Serialize;

pub const MAX_NODES: usize = 128;
pub const MAX_LINKS: usize = 40;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_CHAIN_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Failure {
    Missing,
    DanglingSymlink,
    WrongType,
    AccessDenied,
    SymlinkCycle,
    Unsupported,
    NonUtf8,
    Inspection,
    Limit,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    Special,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub path: String,
    pub kind: NodeKind,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub nodes: Vec<Node>,
    pub resolved: Option<String>,
    pub failure: Option<Failure>,
    pub failed_node: Option<usize>,
    /// Always false: revalidation detects some races, not all of them.
    pub atomic: bool,
    pub change_detected: bool,
    /// False for uncertainty or truncation, not for an observed missing path.
    pub complete: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Step {
    Part(OsString),
    EndLink,
}

struct Observed {
    path: PathBuf,
    metadata: Option<Metadata>,
    error: Option<io::ErrorKind>,
    target: Option<PathBuf>,
}

/// Inspect metadata/link targets only. Synchronous OS calls belong on a bounded
/// blocking worker; no retry can turn this observation into an atomic snapshot.
pub fn inspect(path: &Path) -> Inspection {
    inspect_with(path, || {})
}

fn inspect_with(path: &Path, before_recheck: impl FnOnce()) -> Inspection {
    let mut report = Inspection {
        nodes: vec![],
        resolved: None,
        failure: None,
        failed_node: None,
        atomic: false,
        change_detected: false,
        complete: true,
    };
    let mut observed = vec![];
    let result = walk(path, &mut report, &mut observed);
    if let Err(failure) = result {
        report.failure = Some(failure);
        report.complete = !matches!(
            failure,
            Failure::Unsupported | Failure::NonUtf8 | Failure::Inspection | Failure::Limit
        );
    }
    before_recheck();
    for observation in observed {
        let changed = match (
            observation.metadata,
            fs::symlink_metadata(&observation.path),
        ) {
            (Some(before), Ok(after)) => {
                !same_metadata(&before, &after)
                    || observation.target.is_some_and(|target| {
                        fs::read_link(&observation.path).ok().as_ref() != Some(&target)
                    })
            }
            (None, Err(error)) => observation.error != Some(error.kind()),
            _ => true,
        };
        if changed {
            report.change_detected = true;
            report.complete = false;
        }
    }
    report
}

fn walk(path: &Path, report: &mut Inspection, observed: &mut Vec<Observed>) -> Result<(), Failure> {
    check_path(path)?;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| Failure::Inspection)?
            .join(path)
    };
    check_path(&absolute)?;
    let mut pending = steps(&absolute)?;
    let mut resolved = PathBuf::new();
    let mut links = 0;
    let mut active_links = 0;
    let mut seen = HashSet::new();
    let mut bytes = 2;
    while let Some(step) = pending.pop_front() {
        let Step::Part(part) = step else {
            active_links -= 1;
            continue;
        };
        // All intermediate nodes have already been checked as directories.
        // Resolve '..' only AFTER following links in the preceding component.
        if part == ".." {
            resolved.pop();
        } else if part != "." {
            resolved.push(&part);
        }
        check_path(&resolved)?;
        if report.nodes.len() == MAX_NODES {
            return Err(Failure::Limit);
        }
        let mut node = Node {
            path: resolved.to_str().ok_or(Failure::NonUtf8)?.into(),
            kind: NodeKind::Unknown,
            target: None,
        };
        let metadata = match fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) => {
                observed.push(Observed {
                    path: resolved.clone(),
                    metadata: None,
                    error: Some(error.kind()),
                    target: None,
                });
                push_node(report, node, &mut bytes)?;
                report.failed_node = Some(report.nodes.len() - 1);
                return Err(classify(&error, active_links > 0));
            }
        };
        node.kind = kind(&metadata);
        let is_link = node.kind == NodeKind::Symlink;
        let mut observation = Observed {
            path: resolved.clone(),
            metadata: Some(metadata),
            error: None,
            target: None,
        };
        let target = if is_link {
            Some(fs::read_link(&resolved))
        } else {
            None
        };
        if let Some(Ok(target)) = &target {
            observation.target = Some(target.clone());
            // Invalid encodings are never silently substituted in identifiers.
            // The node stays visible; its unavailable target is explicit.
            if let Ok(()) = check_path(target) {
                node.target = target.to_str().map(str::to_owned);
            }
        }
        observed.push(observation);
        push_node(report, node, &mut bytes)?;
        let index = report.nodes.len() - 1;
        report.failed_node = Some(index);
        if let Some(target) = target {
            let target = target.map_err(|error| classify(&error, true))?;
            check_path(&target)?;
            links += 1;
            let remaining: Vec<_> = pending
                .iter()
                .filter_map(|s| match s {
                    Step::Part(p) => Some(p.clone()),
                    Step::EndLink => None,
                })
                .collect();
            if !seen.insert((resolved.clone(), remaining)) {
                return Err(Failure::SymlinkCycle);
            }
            if links > MAX_LINKS {
                return Err(Failure::Limit);
            }
            let mut target_steps = steps(&target)?;
            if target.is_absolute() {
                resolved.clear();
            } else {
                // Rooted but drive-relative Windows link targets cannot be
                // safely interpreted with the current drive's implicit state.
                if target.has_root() {
                    return Err(Failure::Unsupported);
                }
                resolved.pop();
            }
            target_steps.push_back(Step::EndLink);
            target_steps.append(&mut pending);
            if target_steps.len() > MAX_NODES + MAX_LINKS {
                return Err(Failure::Limit);
            }
            pending = target_steps;
            active_links += 1;
        } else {
            if report.nodes[index].kind == NodeKind::Unknown {
                return Err(Failure::Unsupported);
            }
            if pending.iter().any(|s| matches!(s, Step::Part(_)))
                && report.nodes[index].kind != NodeKind::Directory
            {
                return Err(Failure::WrongType);
            }
        }
        report.failed_node = None;
    }
    report.resolved = resolved.to_str().map(str::to_owned);
    Ok(())
}

fn push_node(report: &mut Inspection, node: Node, bytes: &mut usize) -> Result<(), Failure> {
    // Bound the actual escaped representation, not just raw filesystem bytes.
    *bytes += serde_json::to_vec(&node)
        .expect("string-only node serialization")
        .len()
        + 1;
    if *bytes > MAX_CHAIN_BYTES {
        return Err(Failure::Limit);
    }
    report.nodes.push(node);
    Ok(())
}

fn check_path(path: &Path) -> Result<(), Failure> {
    if path.as_os_str().len() > MAX_PATH_BYTES {
        return Err(Failure::Limit);
    }
    if path.to_str().is_none() {
        return Err(Failure::NonUtf8);
    }
    Ok(())
}

fn steps(path: &Path) -> Result<VecDeque<Step>, Failure> {
    let mut parts = VecDeque::new();
    for component in path.components() {
        // DOS and verbatim paths normalize dot segments differently. Do not
        // present Unix-style link/.. resolution as Windows filesystem evidence.
        #[cfg(windows)]
        if matches!(component, Component::ParentDir)
            || (matches!(component, Component::CurDir)
                && matches!(path.components().next(), Some(Component::Prefix(p)) if p.kind().is_verbatim()))
        {
            return Err(Failure::Unsupported);
        }
        // Prefix+root must be one observation (C: alone is drive-relative).
        if let Component::Prefix(prefix) = component {
            #[cfg(windows)]
            if !matches!(
                prefix.kind(),
                std::path::Prefix::Disk(_)
                    | std::path::Prefix::VerbatimDisk(_)
                    | std::path::Prefix::UNC(_, _)
                    | std::path::Prefix::VerbatimUNC(_, _)
            ) {
                return Err(Failure::Unsupported);
            }
            let mut root = PathBuf::from(prefix.as_os_str());
            if !path.has_root() {
                return Err(Failure::Unsupported);
            }
            root.push(std::path::MAIN_SEPARATOR_STR);
            parts.push_back(Step::Part(root.into_os_string()));
        } else if matches!(component, Component::RootDir)
            && matches!(path.components().next(), Some(Component::Prefix(_)))
        {
            continue;
        } else {
            parts.push_back(Step::Part(component.as_os_str().to_owned()));
        }
        if parts.len() > MAX_NODES {
            return Err(Failure::Limit);
        }
    }
    // Unix '/.' requires a directory; ordinary Windows paths normalize it
    // away. Explicit dot segments in verbatim Windows paths were rejected above.
    let text = path.to_str().ok_or(Failure::NonUtf8)?;
    let separator = |c| c == '/' || (cfg!(windows) && c == '\\');
    if text.ends_with(separator)
        || (!cfg!(windows)
            && text
                .strip_suffix('.')
                .is_some_and(|s| s.ends_with(separator)))
    {
        parts.push_back(Step::Part(".".into()));
    }
    Ok(parts)
}

fn kind(metadata: &Metadata) -> NodeKind {
    if metadata.file_type().is_symlink() {
        return NodeKind::Symlink;
    }
    // Unknown Windows reparse points are not ordinary files/directories.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return NodeKind::Unknown;
        }
    }
    if metadata.is_dir() {
        NodeKind::Directory
    } else if metadata.is_file() {
        NodeKind::File
    } else {
        NodeKind::Special
    }
}

fn classify(error: &io::Error, in_target: bool) -> Failure {
    match error.kind() {
        io::ErrorKind::NotFound if in_target => Failure::DanglingSymlink,
        io::ErrorKind::NotFound => Failure::Missing,
        io::ErrorKind::NotADirectory => Failure::WrongType,
        io::ErrorKind::PermissionDenied => Failure::AccessDenied,
        io::ErrorKind::Unsupported => Failure::Unsupported,
        _ => Failure::Inspection,
    }
}

fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (a.dev(), a.ino(), a.mode()) != (b.dev(), b.ino(), b.mode()) {
            return false;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if (a.file_attributes(), a.creation_time()) != (b.file_attributes(), b.creation_time()) {
            return false;
        }
    }
    // Directory child activity is not a replacement of the traversed node.
    a.file_type() == b.file_type()
        && (a.is_dir() || (a.len() == b.len() && a.modified().ok() == b.modified().ok()))
}

#[cfg(test)]
mod tests;
