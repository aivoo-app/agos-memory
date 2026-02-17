//! `backup` — write a verified snapshot copy of the database (`VACUUM INTO`).
//!
//! Takes the process lock like every other command: one process per database.
//! Stop any running server before backing up, or use the snapshot from a
//! dedicated invocation.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::storage::StoreHandle;

/// Run backup against the resolved config.
pub async fn run(cfg: &Config, out: &Path) -> Result<()> {
    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;

    let report = store.snapshot_to(out).await?;

    println!("snapshot:    {}", report.path.display());
    println!("bytes:       {}", report.bytes);
    println!("integrity:   {}", report.integrity);
    for (table, n) in &report.tables {
        println!("  {table}: {n}");
    }
    println!("backup complete");
    Ok(())
}
