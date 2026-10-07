use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use crate::config::{PathRequirement, PathRequirementType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRequirementFailureKind {
    Missing,
    WrongType,
    NotReadable,
    NotSymlink,
    DanglingSymlink,
    SymlinkCycle,
    Unsupported,
    Inspection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRequirementFailure {
    pub path: PathBuf,
    pub requirement: PathRequirementType,
    pub kind: PathRequirementFailureKind,
    pub detail: Option<io::ErrorKind>,
}

impl PathRequirementFailure {
    pub fn summary(&self) -> &'static str {
        match self.kind {
            PathRequirementFailureKind::Missing => "required path does not exist",
            PathRequirementFailureKind::WrongType => "path has the wrong type",
            PathRequirementFailureKind::NotReadable => "required path is not readable",
            PathRequirementFailureKind::NotSymlink => "path is not a symbolic link",
            PathRequirementFailureKind::DanglingSymlink => "symbolic link target does not exist",
            PathRequirementFailureKind::SymlinkCycle => "symbolic link cycle detected",
            PathRequirementFailureKind::Unsupported => {
                "symbolic link behavior is unsupported on this platform"
            }
            PathRequirementFailureKind::Inspection => "path could not be inspected",
        }
    }
}

pub fn resolve_path(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.unwrap_or_else(|| Path::new(".")).join(path)
    }
}

pub fn evaluate(
    requirement: &PathRequirement,
    cwd: Option<&Path>,
) -> Result<PathBuf, PathRequirementFailure> {
    let path = resolve_path(&requirement.path, cwd);
    let fail = |kind, detail| PathRequirementFailure {
        path: path.clone(),
        requirement: requirement.kind,
        kind,
        detail,
    };

    match requirement.kind {
        PathRequirementType::File => {
            let metadata = fs::metadata(&path).map_err(|error| classify(error, &fail))?;
            if !metadata.is_file() {
                return Err(fail(PathRequirementFailureKind::WrongType, None));
            }
            File::open(&path).map_err(|error| {
                let detail = error.kind();
                let kind = if detail == io::ErrorKind::PermissionDenied {
                    PathRequirementFailureKind::NotReadable
                } else {
                    classify(error, &fail).kind
                };
                fail(kind, Some(detail))
            })?;
        }
        PathRequirementType::Directory => {
            let metadata = fs::metadata(&path).map_err(|error| classify(error, &fail))?;
            if !metadata.is_dir() {
                return Err(fail(PathRequirementFailureKind::WrongType, None));
            }
            fs::read_dir(&path).map_err(|error| {
                let detail = error.kind();
                let kind = if detail == io::ErrorKind::PermissionDenied {
                    PathRequirementFailureKind::NotReadable
                } else {
                    classify(error, &fail).kind
                };
                fail(kind, Some(detail))
            })?;
        }
        PathRequirementType::Symlink => {
            let link = fs::symlink_metadata(&path).map_err(|error| classify(error, &fail))?;
            if !link.file_type().is_symlink() {
                return Err(fail(PathRequirementFailureKind::NotSymlink, None));
            }
            let target = fs::metadata(&path).map_err(|error| {
                let detail = error.kind();
                let kind = if detail == io::ErrorKind::NotFound {
                    PathRequirementFailureKind::DanglingSymlink
                } else {
                    classify(error, &fail).kind
                };
                fail(kind, Some(detail))
            })?;
            if !target.is_file() && !target.is_dir() {
                return Err(fail(PathRequirementFailureKind::WrongType, None));
            }
            let access = if target.is_file() {
                File::open(&path).map(|_| ())
            } else {
                fs::read_dir(&path).map(|_| ())
            };
            access.map_err(|error| {
                let detail = error.kind();
                let kind = if detail == io::ErrorKind::PermissionDenied {
                    PathRequirementFailureKind::NotReadable
                } else {
                    classify(error, &fail).kind
                };
                fail(kind, Some(detail))
            })?;
        }
    }
    Ok(path)
}

fn classify(
    error: io::Error,
    fail: &impl Fn(PathRequirementFailureKind, Option<io::ErrorKind>) -> PathRequirementFailure,
) -> PathRequirementFailure {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => PathRequirementFailureKind::Missing,
        io::ErrorKind::PermissionDenied => PathRequirementFailureKind::NotReadable,
        io::ErrorKind::Unsupported => PathRequirementFailureKind::Unsupported,
        _ if is_symlink_cycle(&error) => PathRequirementFailureKind::SymlinkCycle,
        _ => PathRequirementFailureKind::Inspection,
    };
    fail(kind, Some(error.kind()))
}

#[cfg(unix)]
fn is_symlink_cycle(error: &io::Error) -> bool {
    error.raw_os_error() == Some(nix::libc::ELOOP)
}

#[cfg(windows)]
fn is_symlink_cycle(error: &io::Error) -> bool {
    error.raw_os_error() == Some(1921) // ERROR_CANT_RESOLVE_FILENAME
}

#[cfg(not(any(unix, windows)))]
fn is_symlink_cycle(_: &io::Error) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use std::{os::unix::fs::symlink, path::Path};

    use tempfile::tempdir;

    use super::*;

    fn requirement(kind: PathRequirementType, path: impl Into<PathBuf>) -> PathRequirement {
        PathRequirement {
            kind,
            path: path.into(),
        }
    }

    #[test]
    fn test_evaluate_file_directory_and_relative_paths() {
        let root = tempdir().unwrap();
        let file = root.path().join("data.txt");
        fs::write(&file, "secret content").unwrap();
        fs::create_dir(root.path().join("data")).unwrap();
        assert_eq!(
            evaluate(
                &requirement(PathRequirementType::File, "data.txt"),
                Some(root.path())
            )
            .unwrap(),
            file
        );
        assert!(evaluate(
            &requirement(PathRequirementType::Directory, "data"),
            Some(root.path())
        )
        .is_ok());
        assert!(evaluate(
            &requirement(PathRequirementType::File, root.path().join("data.txt")),
            None
        )
        .is_ok());
    }

    #[test]
    fn test_evaluate_reports_missing_wrong_type_and_unreadable_paths() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        let missing = evaluate(
            &requirement(PathRequirementType::File, "missing"),
            Some(root.path()),
        )
        .unwrap_err();
        assert_eq!(missing.kind, PathRequirementFailureKind::Missing);
        let wrong = evaluate(
            &requirement(PathRequirementType::File, "directory"),
            Some(root.path()),
        )
        .unwrap_err();
        assert_eq!(wrong.kind, PathRequirementFailureKind::WrongType);

        let file = root.path().join("unreadable");
        fs::write(&file, "secret").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o000);
        fs::set_permissions(&file, permissions).unwrap();
        if unsafe { nix::libc::geteuid() } != 0 {
            let error = evaluate(
                &requirement(PathRequirementType::File, "unreadable"),
                Some(root.path()),
            )
            .unwrap_err();
            assert_eq!(error.kind, PathRequirementFailureKind::NotReadable);
        }
    }

    #[test]
    fn test_evaluate_symlink_distinguishes_non_link_dangling_and_cycles() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("target"), "content").unwrap();
        symlink("target", root.path().join("link")).unwrap();
        symlink("absent", root.path().join("dangling")).unwrap();
        symlink("cycle-b", root.path().join("cycle-a")).unwrap();
        symlink("cycle-a", root.path().join("cycle-b")).unwrap();
        assert!(evaluate(
            &requirement(PathRequirementType::Symlink, "link"),
            Some(root.path())
        )
        .is_ok());
        assert_eq!(
            evaluate(
                &requirement(PathRequirementType::Symlink, "target"),
                Some(root.path())
            )
            .unwrap_err()
            .kind,
            PathRequirementFailureKind::NotSymlink
        );
        assert_eq!(
            evaluate(
                &requirement(PathRequirementType::Symlink, "dangling"),
                Some(root.path())
            )
            .unwrap_err()
            .kind,
            PathRequirementFailureKind::DanglingSymlink
        );
        assert_eq!(
            evaluate(
                &requirement(PathRequirementType::Symlink, "cycle-a"),
                Some(root.path())
            )
            .unwrap_err()
            .kind,
            PathRequirementFailureKind::SymlinkCycle
        );
    }

    #[test]
    fn test_resolve_absolute_path_ignores_working_directory() {
        assert_eq!(
            resolve_path(Path::new("/absolute"), Some(Path::new("/cwd"))),
            PathBuf::from("/absolute")
        );
    }
}
