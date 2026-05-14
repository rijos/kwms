//! `kwms serve` — start the HTTP server.

use std::path::PathBuf;

pub async fn run(_config: PathBuf, _secrets: PathBuf) -> anyhow::Result<()> {
    todo!("phase 2 (server agent): load config, build AppState, start axum app")
}
