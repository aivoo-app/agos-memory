//! sqlite-vec extension registration (issue 0007).
//!
//! `sqlite-vec` compiles the extension's C source into a static library
//! (`sqlite_vec0`); the entry point `sqlite3_vec_init` must be registered via
//! `sqlite3_auto_extension` **once per process** so every new connection
//! (writer, read pool, migrations) gets the `vec0` virtual table.
//!
//! Registration is idempotent and thread-safe (guarded by `OnceLock`).
//!
//! The upstream crate declares `sqlite3_vec_init` as `extern "C" { fn
//! sqlite3_vec_init(); }` and transmutes it to the `sqlite3_auto_extension`
//! signature at call time. We follow the same pattern: the entry point is
//! re-exported from `crate::sqlite_vec` (which mirrors `crates/sqlite-vec-src`'s
//! linkage), and we transmute on registration exactly as upstream's rusqlite
//! integration test does.

use std::sync::OnceLock;

use rusqlite::ffi::sqlite3_auto_extension;

use crate::error::{Error, Result};
use crate::sqlite_vec::sqlite3_vec_init;

static REGISTER: OnceLock<std::result::Result<(), String>> = OnceLock::new();

/// Register the vec0 module for all future connections. Idempotent.
pub fn register() -> Result<()> {
    let res = REGISTER.get_or_init(|| {
        // SAFETY: `sqlite3_vec_init` is the extension's documented entry point
        // (same pattern as upstream's rusqlite integration test). The auto
        // extension list expects the standard `sqlite3_<name>_init` signature.
        // sqlite-vec's entry point matches it at the C ABI level; transmuting the
        // Rust-declared no-arg pointer to the auto-extension signature is what
        // upstream does and what makes registration work with rusqlite.
        unsafe {
            type InitFunc = unsafe extern "C" fn(
                db: *mut rusqlite::ffi::sqlite3,
                pz_err_msg: *mut *mut std::os::raw::c_char,
                p_api: *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int;
            let init: Option<InitFunc> = Some(std::mem::transmute(sqlite3_vec_init as *const ()));
            sqlite3_auto_extension(init);
        }
        Ok(())
    });

    match res {
        Ok(()) => Ok(()),
        Err(msg) => Err(Error::Storage(msg.clone())),
    }
}

/// Verify the extension actually works on a fresh connection.
pub fn verify(conn: &rusqlite::Connection) -> Result<String> {
    let v = conn
        .query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0))
        .map_err(|e| {
            Error::Storage(format!(
                "sqlite-vec not available: {e}; is the extension compiled in?"
            ))
        })?;
    Ok(v)
}
