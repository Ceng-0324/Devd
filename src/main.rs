use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "devd", version, about = "Manage local development services")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a devd configuration file.
    Init,
    /// Start all configured services.
    Start,
    /// Stop all managed services.
    Stop,
    /// Restart one managed service.
    Restart { service: String },
    /// Show managed service status.
    Status,
    /// Show service logs.
    Logs { service: Option<String> },
    /// Validate the configuration file.
    Check,
    /// Display the service dependency graph.
    Graph,
    /// Show resource usage for managed services.
    Top,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Command implementations will be added in the MVP workstream.
    match cli.command {
        Command::Init
        | Command::Start
        | Command::Stop
        | Command::Restart { .. }
        | Command::Status
        | Command::Logs { .. }
        | Command::Check
        | Command::Graph
        | Command::Top => {}
    }

    Ok(())
}
