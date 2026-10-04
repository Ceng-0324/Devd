#[cfg(unix)]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use clap::Parser;
    match devd::cli::Cli::parse().run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("error: devd currently supports Linux and macOS");
    std::process::ExitCode::FAILURE
}
