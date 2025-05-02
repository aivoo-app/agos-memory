//! Root clap definition and command dispatch.

use clap::{Parser, Subcommand};
use rusqlite::Connection;

use crate::config::Config;
use crate::error::Result;
use crate::storage::schema;
use crate::storage::vecext;
use crate::{NAME, VERSION, defaults};

/// Self-hosted agent memory manager.
#[derive(Debug, Parser)]
#[command(name = NAME, version = VERSION, about, arg_required_else_help = true)]
pub struct Cli {
    /// Path to the config file (defaults to ./agos-memory.toml when present).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Path to the SQLite database (overrides config).
    #[arg(long, global = true)]
    pub db: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// Available commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the database and config scaffold; safe to re-run.
    Init {
        /// Force overwriting an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// Show database health: schema, memory counts.
    Status,
    /// Deeper diagnostics: sqlite-vec, FTS5, pragmas, integrity.
    Doctor,
    /// Write a verified snapshot copy of the database.
    Backup {
        /// Output file for the snapshot.
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

/// Parse and run; the caller decides process exit codes.
pub fn run(cli: Cli) -> Result<()> {
    let cfg = {
        let mut cfg = Config::load(cli.config.as_deref().map(std::path::Path::new))?;
        if let Some(db) = &cli.db {
            cfg.db_path = db.into();
        }
        cfg.validate()?;
        cfg
    };
    match cli.command {
        Command::Init { force } => super::init::run(&cfg, force),
        Command::Status => status(&cfg),
        Command::Doctor => doctor(&cfg),
        Command::Backup { out } => super::backup::run(&cfg, &out),
    }
}

/// `status` — quick health summary.
fn status(cfg: &Config) -> Result<()> {
    if !cfg.db_path.is_file() {
        println!(
            "no database at {} — run `agos-memory init`",
            cfg.db_path.display()
        );
        return Ok(());
    }
    let conn =
        Connection::open_with_flags(&cfg.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version = schema::user_version(&conn)?;
    let mut stmt =
        conn.prepare("SELECT status, count(*) FROM memories GROUP BY status ORDER BY status")?;
    let counts = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    println!("db:       {}", cfg.db_path.display());
    println!("agent:    {}", cfg.agent_id);
    println!(
        "schema:   v{version} (supported v{})",
        schema::SCHEMA_VERSION
    );
    if counts.is_empty() {
        println!("memories: (none)");
    } else {
        for (status, n) in counts {
            println!("memories[{status}]: {n}");
        }
    }
    Ok(())
}

/// `doctor` — deeper checks.
fn doctor(cfg: &Config) -> Result<()> {
    println!("doctor: {}", cfg.db_path.display());

    if !cfg.db_path.is_file() {
        println!("  [FAIL] database missing — run `agos-memory init`");
        return Err(crate::error::Error::Storage(format!(
            "database {} does not exist",
            cfg.db_path.display()
        )));
    }

    // Register sqlite-vec BEFORE opening the connection so the extension
    // is available on this connection (sqlite3_auto_extension only affects
    // future connections).
    vecext::register()?;

    let conn = Connection::open(&cfg.db_path)?;
    schema::apply_pragmas(&conn)?;

    let v = schema::user_version(&conn)?;
    let ok_schema = v <= schema::SCHEMA_VERSION;
    println!(
        "  [{}] schema v{v} (supported v{})",
        if ok_schema { " OK " } else { "FAIL" },
        schema::SCHEMA_VERSION
    );
    if !ok_schema {
        return Err(crate::error::Error::SchemaTooNew {
            db: v,
            supported: schema::SCHEMA_VERSION,
        });
    }

    match vecext::verify(&conn) {
        Ok(version) => println!("  [ OK ] sqlite-vec {version}"),
        Err(e) => {
            println!("  [FAIL] sqlite-vec: {e}");
            return Err(e);
        }
    }

    let fts5: String = conn
        .query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })
        .unwrap_or_else(|_| "0".into());
    println!(
        "  [{}] fts5 available",
        if fts5 == "1" { " OK " } else { "FAIL" }
    );

    let integrity = schema::integrity_check(&conn)?;
    println!(
        "  [{}] integrity: {integrity}",
        if integrity == "ok" { " OK " } else { "FAIL" }
    );

    let fk: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    println!(
        "  [{}] foreign_keys = {fk}",
        if fk == 1 { " OK " } else { "FAIL" }
    );

    println!("doctor done");
    Ok(())
}

// `defaults` is re-exported for binary consumers; keep the import referenced.
#[allow(unused_imports)]
use defaults as _defaults;
