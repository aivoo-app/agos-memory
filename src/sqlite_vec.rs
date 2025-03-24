//! Re-export of the sqlite-vec C entry point, wired through `crates/sqlite-vec-src`.
//!
//! `crates/sqlite-vec-src` is a local crate whose `Cargo.toml` names the library
//! `sqlite_vec` and whose `src/lib.rs` carries `#[link(name = "sqlite_vec0")]`.
//! This module mirrors that linkage so `extern "C" { fn sqlite3_vec_init() }`
//! resolves to the static archive built from `sqlite-vec.c` (compiled with
//! `-DSQLITE_CORE`).

#[link(name = "sqlite_vec0")]
unsafe extern "C" {
    /// Auto-extension entry point for the `vec0` virtual table.
    /// Register it via `sqlite3_auto_extension` once per process; every new
    /// connection (writer, read pool, migrations) then sees `vec0`.
    pub fn sqlite3_vec_init();
}
