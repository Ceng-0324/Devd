use crate::{
    config::{ConfigError, HealthCheck, ServiceConfig},
    core::path_requirements::{evaluate, PathRequirementFailure},
    core::process_manager::{
        environment_file_path, load_environment_file, parse_command, ProcessError,
    },
};
use anyhow::{bail, Result};
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

const DOCTOR_SCHEMA_VERSION: u16 = 1;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Print the report as JSON.
    #[arg(long)]
    pub(super) json: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum CheckStatus {
    Passed,
    Warning,
    Failed,
    NotChecked,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Warning => "warning",
            Self::Failed => "failed",
            Self::NotChecked => "not-checked",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    service: Option<String>,
    check: String,
    status: CheckStatus,
    summary: String,
    evidence: Vec<String>,
    recommendation: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    schema_version: u16,
    config: PathBuf,
    profile: Option<String>,
    ready: bool,
    checks: Vec<DoctorCheck>,
}

pub(super) async fn run(args: Args, config_path: &Path, profile: Option<&str>) -> Result<()> {
    let config = match super::load_config(config_path, profile).await {
        Ok(config) => config,
        Err(error) => {
            let report = DoctorReport {
                schema_version: DOCTOR_SCHEMA_VERSION,
                config: config_path.to_owned(),
                profile: profile.map(str::to_owned),
                ready: false,
                checks: vec![check(
                    None,
                    "configuration",
                    CheckStatus::Failed,
                    "Configuration could not be loaded or validated.",
                    vec![safe_config_error(&error)],
                    Some("Run `devd check` for the configuration diagnostic."),
                )],
            };
            super::output(&render(&report, args.json)?)?;
            bail!("doctor found configuration errors");
        }
    };

    let mut checks = Vec::new();
    let mut service_names: Vec<_> = config.services.keys().collect();
    service_names.sort();
    for name in service_names {
        let service = &config.services[name];
        check_working_directory(name, service, &mut checks);
        for requirement in &service.requires {
            match evaluate(requirement, service.cwd.as_deref()) {
                Ok(path) => checks.push(check(
                    Some(name),
                    required_check_name(requirement.kind),
                    CheckStatus::Passed,
                    "Required path exists, has the declared type, and is readable.",
                    vec![path.display().to_string()],
                    None,
                )),
                Err(error) => checks.push(path_requirement_failure(name, error)),
            }
        }

        let environment = match load_environment_file(name, service).await {
            Ok(environment) => {
                if let Some(path) = environment_file_path(service) {
                    checks.push(check(
                        Some(name),
                        "env-file",
                        CheckStatus::Passed,
                        "Environment file is readable and has valid dotenv syntax.",
                        vec![path.display().to_string()],
                        None,
                    ));
                }
                environment
            }
            Err(error) => {
                checks.push(environment_failure(name, error));
                Vec::new()
            }
        };
        check_program(
            name,
            "command",
            &service.command,
            service,
            &environment,
            &mut checks,
        );
        if let Some(HealthCheck::Script { command, .. }) = &service.healthcheck {
            check_program(
                name,
                "script-probe",
                command,
                service,
                &environment,
                &mut checks,
            );
        }
        for address in &service.listen {
            checks.push(check_listen_address(name, *address).await);
        }
    }
    if config
        .services
        .values()
        .all(|service| service.listen.is_empty())
    {
        checks.push(check(
            None,
            "listen-addresses",
            CheckStatus::NotChecked,
            "No service listening addresses are declared; healthcheck endpoints are not treated as owned ports.",
            Vec::new(),
            Some("Declare service-owned TCP addresses with `listen` to check bind availability."),
        ));
    }

    let report = DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        config: config_path.to_owned(),
        profile: profile.map(str::to_owned),
        ready: !checks
            .iter()
            .any(|check| check.status == CheckStatus::Failed),
        checks,
    };
    let failed = !report.ready;
    super::output(&render(&report, args.json)?)?;
    if failed {
        bail!("doctor found environment failures");
    }
    Ok(())
}

fn required_check_name(kind: crate::config::PathRequirementType) -> &'static str {
    match kind {
        crate::config::PathRequirementType::File => "required-file",
        crate::config::PathRequirementType::Directory => "required-directory",
        crate::config::PathRequirementType::Symlink => "required-symlink",
    }
}

fn path_requirement_failure(service: &str, failure: PathRequirementFailure) -> DoctorCheck {
    let kind = required_check_name(failure.requirement);
    let mut evidence = vec![failure.path.display().to_string()];
    if let Some(detail) = failure.detail {
        evidence.push(format!("io-error={detail}"));
    }
    check(
        Some(service),
        kind,
        CheckStatus::Failed,
        failure.summary(),
        evidence,
        Some("Create or correct the required path and ensure the service user can access it."),
    )
}

fn check_working_directory(name: &str, service: &ServiceConfig, checks: &mut Vec<DoctorCheck>) {
    let Some(path) = service.cwd.as_deref() else {
        checks.push(check(
            Some(name),
            "working-directory",
            CheckStatus::NotChecked,
            "No working directory is configured.",
            Vec::new(),
            None,
        ));
        return;
    };
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_dir() => checks.push(check(
            Some(name),
            "working-directory",
            CheckStatus::Failed,
            "Configured working directory is not a directory.",
            vec![path.display().to_string()],
            Some("Set `cwd` to an existing directory."),
        )),
        Ok(_) => {
            #[cfg(unix)]
            let access =
                nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).map_err(io::Error::from);
            #[cfg(not(unix))]
            let access: io::Result<()> = Ok(());
            match access {
                Ok(()) => checks.push(check(
                    Some(name),
                    "working-directory",
                    CheckStatus::Passed,
                    "Configured working directory exists and is a directory.",
                    vec![path.display().to_string()],
                    None,
                )),
                Err(error) => checks.push(check(
                    Some(name),
                    "working-directory",
                    CheckStatus::Failed,
                    "Configured working directory cannot be traversed by this user.",
                    vec![format!("{} ({})", path.display(), error.kind())],
                    Some("Check directory search permissions for the current user."),
                )),
            }
        }
        Err(error) => checks.push(check(
            Some(name),
            "working-directory",
            CheckStatus::Failed,
            "Configured working directory cannot be inspected.",
            vec![format!("{} ({})", path.display(), error.kind())],
            Some("Create the directory or correct the `cwd` path."),
        )),
    }
}

fn check_program(
    service_name: &str,
    kind: &str,
    command: &str,
    service: &ServiceConfig,
    file_environment: &[(String, String)],
    checks: &mut Vec<DoctorCheck>,
) {
    let arguments = match parse_command(service_name, command) {
        Ok(arguments) => arguments,
        Err(error) => {
            checks.push(check(
                Some(service_name),
                kind,
                CheckStatus::Failed,
                "Command arguments cannot be parsed with the service command rules.",
                vec![safe_process_error(&error)],
                Some("Correct the shell-style quoting in the command."),
            ));
            return;
        }
    };
    let program = &arguments[0];
    let path_value = environment_value(&service.env, "PATH")
        .or_else(|| environment_file_value(file_environment, "PATH"))
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    match find_program(program, &path_value, service.cwd.as_deref()) {
        Some(path) => checks.push(check(
            Some(service_name),
            kind,
            CheckStatus::Passed,
            "Program is discoverable using the service PATH and working directory.",
            vec![path.display().to_string()],
            None,
        )),
        None => checks.push(check(
            Some(service_name),
            kind,
            CheckStatus::Failed,
            "Program could not be found or is not executable.",
            vec![program.clone()],
            Some("Install the program or correct its command/PATH."),
        )),
    }
}

fn environment_value(environment: &HashMap<String, String>, key: &str) -> Option<String> {
    #[cfg(windows)]
    return environment
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone());
    #[cfg(not(windows))]
    environment.get(key).cloned()
}

fn environment_file_value(environment: &[(String, String)], key: &str) -> Option<String> {
    #[cfg(windows)]
    return environment
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.clone());
    #[cfg(not(windows))]
    environment
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

fn find_program(program: &str, path: &OsStr, cwd: Option<&Path>) -> Option<PathBuf> {
    let program_path = Path::new(program);
    let has_path = program_path.is_absolute() || program_path.components().count() > 1;
    if has_path {
        let candidate = if program_path.is_absolute() {
            program_path.to_owned()
        } else {
            cwd.unwrap_or(Path::new(".")).join(program_path)
        };
        return executable_file(&candidate).then_some(candidate);
    }

    let directories = std::env::split_paths(path);
    for directory in directories {
        let directory = if directory.as_os_str().is_empty() {
            cwd.unwrap_or(Path::new(".")).to_owned()
        } else if directory.is_absolute() {
            directory
        } else {
            cwd.unwrap_or(Path::new(".")).join(directory)
        };
        for candidate in executable_candidates(&directory.join(program)) {
            if executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if path.extension().is_some() {
            vec![path.to_owned()]
        } else {
            ["exe", "com"]
                .into_iter()
                .map(|extension| path.with_extension(extension))
                .collect()
        }
    }
    #[cfg(not(windows))]
    {
        vec![path.to_owned()]
    }
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

async fn check_listen_address(service: &str, address: SocketAddr) -> DoctorCheck {
    match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => {
            drop(listener);
            check(
                Some(service),
                "listen-address",
                CheckStatus::Passed,
                "Address was bindable when checked; it has been released.",
                vec![address.to_string()],
                Some("Availability can change before the service starts."),
            )
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => check(
            Some(service),
            "listen-address",
            CheckStatus::Failed,
            "Address is already in use.",
            vec![format!("{address} (os_code={:?})", error.raw_os_error())],
            Some("Find the process using the address or choose another `listen` address."),
        ),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AddrNotAvailable | io::ErrorKind::PermissionDenied
            ) =>
        {
            check(
                Some(service),
                "listen-address",
                CheckStatus::Failed,
                "Address cannot be bound by this user on this host.",
                vec![format!(
                    "{address} ({}, os_code={:?})",
                    error.kind(),
                    error.raw_os_error()
                )],
                Some("Check the local interface address and permission to bind this port."),
            )
        }
        Err(error) => check(
            Some(service),
            "listen-address",
            CheckStatus::Warning,
            "Bind availability could not be determined.",
            vec![format!(
                "{address} ({}, os_code={:?})",
                error.kind(),
                error.raw_os_error()
            )],
            Some("Confirm the address and platform support before starting the service."),
        ),
    }
}

fn environment_failure(service: &str, error: ProcessError) -> DoctorCheck {
    let (path, reason) = match error {
        ProcessError::EnvironmentRead { path, source, .. } => {
            (path, format!("cannot read ({})", source.kind()))
        }
        ProcessError::EnvironmentParse { path, .. } => (path, "invalid dotenv syntax".into()),
        other => {
            return check(
                Some(service),
                "env-file",
                CheckStatus::Failed,
                "Environment file could not be loaded.",
                vec![safe_process_error(&other)],
                Some("Correct the environment file before starting the service."),
            )
        }
    };
    check(
        Some(service),
        "env-file",
        CheckStatus::Failed,
        "Environment file could not be loaded.",
        vec![format!("{}: {reason}", path.display())],
        Some("Make the file readable and correct its dotenv syntax."),
    )
}

fn safe_process_error(error: &ProcessError) -> String {
    match error {
        ProcessError::EmptyCommand { .. } => "empty command".into(),
        ProcessError::CommandParse { .. } => "invalid shell-style command quoting".into(),
        ProcessError::EnvironmentRead { path, source, .. } => {
            format!("{}: cannot read ({})", path.display(), source.kind())
        }
        ProcessError::EnvironmentParse { path, .. } => {
            format!("{}: invalid dotenv syntax", path.display())
        }
        _ => "service command could not be prepared".into(),
    }
}

fn safe_config_error(error: &anyhow::Error) -> String {
    if let Some(error) = error.downcast_ref::<ConfigError>() {
        match error {
            ConfigError::Read { path, source } => {
                format!(
                    "{}: cannot read configuration ({})",
                    path.display(),
                    source.kind()
                )
            }
            ConfigError::Parse { .. } => "configuration YAML is invalid".into(),
            ConfigError::Validation { source, .. } => format!("configuration validation: {source}"),
            ConfigError::Profile { source, .. } => format!("profile configuration error: {source}"),
        }
    } else {
        "configuration could not be loaded".into()
    }
}

fn check(
    service: Option<&str>,
    kind: &str,
    status: CheckStatus,
    summary: &str,
    evidence: Vec<String>,
    recommendation: Option<&str>,
) -> DoctorCheck {
    DoctorCheck {
        service: service.map(str::to_owned),
        check: kind.into(),
        status,
        summary: summary.into(),
        evidence,
        recommendation: recommendation.map(str::to_owned),
    }
}

fn render(report: &DoctorReport, json: bool) -> Result<String> {
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(report)?));
    }
    let mut text = format!(
        "Environment doctor: {}\n",
        safe_text(&report.config.display().to_string())
    );
    if let Some(profile) = &report.profile {
        text.push_str(&format!("Profile: {}\n", safe_text(profile)));
    }
    for check in &report.checks {
        let owner = check
            .service
            .as_deref()
            .map(|name| format!("[{}] ", safe_text(name)))
            .unwrap_or_default();
        text.push_str(&format!(
            "{}: {owner}{} - {}\n",
            check.status.label(),
            check.check,
            safe_text(&check.summary)
        ));
        for evidence in &check.evidence {
            text.push_str(&format!("  evidence: {}\n", safe_text(evidence)));
        }
        if let Some(recommendation) = &check.recommendation {
            text.push_str(&format!("  next: {}\n", safe_text(recommendation)));
        }
    }
    text.push_str(if report.ready {
        "Result: no blocking environment failures found.\n"
    } else {
        "Result: environment failures found.\n"
    });
    Ok(text)
}

fn safe_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() && character != '\t' {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::safe_text;

    #[test]
    fn test_doctor_text_escapes_terminal_controls() {
        assert_eq!(safe_text("path\u{1b}[2J\nnext"), "path\\u{1b}[2J\\nnext");
        assert_eq!(safe_text("tab\tkept"), "tab\tkept");
    }
}
