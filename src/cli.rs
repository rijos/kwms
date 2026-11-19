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
    /// Tile cache maintenance.
    #[command(subcommand)]
    Cache(CacheCommand),
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Render tiles into the durable cache tiers ahead of requests.
    ///
    /// By default every level whose tiles are larger than one source file
    /// (`--max-level auto`): the levels that are slow to render on demand.
    /// Fresh tiles are skipped, so re-running resumes and retries failures.
    Update {
        #[arg(short, long, default_value = "config.yaml")]
        config: PathBuf,
        #[arg(short, long, default_value = "secrets.yaml")]
        secrets: PathBuf,
        /// Layer to update (repeatable; default: all layers).
        #[arg(long = "layer", value_name = "NAME")]
        layers: Vec<String>,
        /// Grid name or WMTS TileMatrixSet (repeatable; default: all of the layer's).
        #[arg(long = "grid", value_name = "NAME")]
        grids: Vec<String>,
        /// Format, e.g. jpeg, png (repeatable; default: all of the layer's).
        #[arg(long = "format", value_name = "FORMAT")]
        formats: Vec<String>,
        /// First level to fill.
        #[arg(long, default_value_t = 0)]
        min_level: u32,
        /// Last level to fill: a number, or `auto` (levels whose tiles are
        /// larger than one source file).
        #[arg(long, default_value = "auto")]
        max_level: crate::subcommand::cache_update::MaxLevel,
        /// Re-render tiles even if a fresh copy is cached.
        #[arg(long)]
        force: bool,
        /// Print the plan (levels and tile counts) and exit.
        #[arg(long)]
        dry_run: bool,
        /// Tiles rendered at once.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Time limit for one attempt at one tile (e.g. 10m, 90s).
        #[arg(long, default_value = "10m", value_parser = humantime_serde::re::humantime::parse_duration)]
        tile_timeout: std::time::Duration,
        /// Retries per tile after an error or timeout (with backoff).
        #[arg(long, default_value_t = 3)]
        retries: u32,
        /// Refuse plans with more tiles than this.
        #[arg(long, default_value_t = 1_000_000)]
        max_tiles: u64,
    },
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
        Command::Serve { config, secrets } => crate::subcommand::serve::run(config, secrets).await,
        Command::Config(ConfigCommand::Check { config, secrets }) => {
            crate::subcommand::config_check::run(config, secrets)
        }
        Command::Config(ConfigCommand::Schema) => crate::subcommand::config_schema::run(),
        Command::Cache(CacheCommand::Update {
            config,
            secrets,
            layers,
            grids,
            formats,
            min_level,
            max_level,
            force,
            dry_run,
            concurrency,
            tile_timeout,
            retries,
            max_tiles,
        }) => {
            crate::subcommand::cache_update::run(crate::subcommand::cache_update::Args {
                config,
                secrets,
                layers,
                grids,
                formats,
                min_level,
                max_level,
                force,
                dry_run,
                concurrency,
                tile_timeout,
                retries,
                max_tiles,
            })
            .await
        }
    }
}
