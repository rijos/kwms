//! `kwms config check` — load + merge + validate, exit non-zero on failure.

use std::path::PathBuf;

pub fn run(_config: PathBuf, _secrets: PathBuf) -> anyhow::Result<()> {
    todo!("phase 2 (config agent): load_and_validate, exit appropriately")
}
