//! Command-line interface — subcommands live under [`crate::subcommand`].

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kwms", version, about = "WM(T)S server for aerial imagery")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP server.
    Serve {
        #[arg(short, long, default_value = "config.yaml")]
        config: PathBuf,
        #[arg(short, long, default_value = "secrets.yaml")]
        secrets: PathBuf,
    },
    /// Config helpers.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Load + merge + validate config files and exit.
    Check {
        #[arg(short, long, default_value = "config.yaml")]
        config: PathBuf,
        #[arg(short, long, default_value = "secrets.yaml")]
        secrets: PathBuf,
    },
    /// Emit JSON Schema for the merged config to stdout.
    Schema,
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config, secrets } => {
            crate::subcommand::serve::run(config, secrets).await
        }
        Command::Config(ConfigCommand::Check { config, secrets }) => {
            crate::subcommand::config_check::run(config, secrets)
        }
        Command::Config(ConfigCommand::Schema) => crate::subcommand::config_schema::run(),
    }
}
