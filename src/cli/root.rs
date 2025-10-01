//! Root clap definition and command dispatch.

use clap::{Parser, Subcommand};
use rusqlite::Connection;

use crate::config::{
    BudgetSplit, Config, RecallConfig, RecallHalfLives, RecallWeights, TrustPolicy,
};
use crate::embed::embedder_from_config;
use crate::error::Result;
use crate::recall::RecallQuery;
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
    /// Store one fact as a durable memory (redacted, embedded, deduped).
    Remember {
        /// The fact text.
        #[arg(long)]
        text: String,
        /// Memory tier (default episodic).
        #[arg(long, default_value = "episodic")]
        tier: String,
        /// Memory kind (default fact).
        #[arg(long, default_value = "fact")]
        kind: String,
        /// Provenance: user/agent/tool/file/web/import (tool/web → untrusted).
        #[arg(long, default_value = "user")]
        source_kind: String,
        /// Extraction confidence 0..1 (below threshold → pending).
        #[arg(long, default_value_t = 0.8)]
        confidence: f64,
    },
    /// Manage sessions and turns.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Run hybrid recall against the agent's memory store.
    Recall {
        /// Query text to search for.
        text: String,
        /// Maximum hits to return (default from config).
        #[arg(long)]
        k: Option<usize>,
        /// Token budget for the context window (default from config).
        #[arg(long)]
        budget: Option<u64>,
        /// Include untrusted memories (fenced, never laundered).
        #[arg(long)]
        include_untrusted: bool,
        /// Include pending memories (importance discounted by confidence).
        #[arg(long)]
        include_pending: bool,
        /// Include episodic memories.
        #[arg(long)]
        include_episodic: bool,
        /// Minimum rerank score to include a hit (default from config).
        #[arg(long)]
        min_score: Option<f64>,
        /// Emit the full RecallReport as JSON instead of fenced blocks.
        #[arg(long)]
        json: bool,
        /// Annotate each cited memory with its score components.
        #[arg(long)]
        explain: bool,
    },
    /// Drill down into one memory's provenance, lineage, and recall history.
    Explain {
        /// Public id of the memory to explain.
        id: String,
    },
    /// Run the offline eval suite against a cases file.
    Eval {
        /// JSONL file with eval cases (id, query, relevant[], forbidden[], corpus[]).
        #[arg(long)]
        file: std::path::PathBuf,
        /// Fail if precision drops below this threshold.
        #[arg(long, default_value_t = 0.90)]
        min_precision: f64,
        /// Fail if recall drops below this threshold.
        #[arg(long, default_value_t = 0.95)]
        min_recall: f64,
        /// Fail if MRR drops below this threshold.
        #[arg(long, default_value_t = 0.80)]
        min_mrr: f64,
        /// Emit Metrics as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Session subcommands.
#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// Open a new session for this agent.
    Open,
    /// Append a turn (defaults to the open session).
    Append {
        /// Session public id (defaults to the open session).
        #[arg(long)]
        session: Option<String>,
        /// Turn role: user/assistant/system/tool.
        #[arg(long, default_value = "user")]
        role: String,
        /// Turn content.
        #[arg(long)]
        content: String,
    },
    /// Close a session (defaults to the open session).
    Close {
        /// Session public id (defaults to the open session).
        session: Option<String>,
    },
    /// Close sessions idle longer than the configured timeout.
    IdleClose,
}

/// Parse and run; the caller decides process exit codes.
pub async fn run(cli: Cli) -> Result<()> {
    let cfg = {
        let mut cfg = Config::load(cli.config.as_deref().map(std::path::Path::new))?;
        if let Some(db) = &cli.db {
            cfg.db_path = db.into();
        }
        cfg.validate()?;
        cfg
    };
    match cli.command {
        Command::Init { force } => super::init::run(&cfg, force).await,
        Command::Status => status(&cfg).await,
        Command::Doctor => doctor(&cfg).await,
        Command::Backup { out } => super::backup::run(&cfg, &out).await,
        Command::Remember {
            text,
            tier,
            kind,
            source_kind,
            confidence,
        } => {
            super::remember::run_remember(&cfg, &text, &tier, &kind, &source_kind, confidence).await
        }
        Command::Session { cmd } => super::remember::run_session(&cfg, &cmd).await,
        Command::Recall {
            text,
            k,
            budget,
            include_untrusted,
            include_pending,
            include_episodic,
            min_score,
            json,
            explain,
        } => {
            recall_cmd(
                &cfg,
                text,
                k,
                budget,
                include_untrusted,
                include_pending,
                include_episodic,
                min_score,
                json,
                explain,
            )
            .await
        }
        Command::Explain { id } => explain_cmd(&cfg, id).await,
        Command::Eval {
            file,
            min_precision,
            min_recall,
            min_mrr,
            json,
        } => eval_cmd(&cfg, file, min_precision, min_recall, min_mrr, json).await,
    }
}

/// `status` — quick health summary.
async fn status(cfg: &Config) -> Result<()> {
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
async fn doctor(cfg: &Config) -> Result<()> {
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

    let fts5: i64 = conn
        .query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    println!(
        "  [{}] fts5 available",
        if fts5 == 1 { " OK " } else { "FAIL" }
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

/// `recall` CLI command — runs hybrid recall and prints results.
async fn recall_cmd(
    cfg: &Config,
    text: String,
    k: Option<usize>,
    budget: Option<u64>,
    include_untrusted: bool,
    include_pending: bool,
    include_episodic: bool,
    min_score: Option<f64>,
    json: bool,
    explain: bool,
) -> Result<()> {
    // Open store with recall embedder
    let store = crate::storage::StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await?;

    // Get embedding dimension from store (pinned in meta at init)
    let dim = store.embed_dim().await?;

    // Build embedder from config
    let embedder = crate::embed::embedder_from_config(&cfg.embed, dim);
    store.validate_embed_dim(&*embedder).await?;

    // Build RecallQuery with overrides
    let recall_cfg = RecallConfig {
        top_k: k.unwrap_or(cfg.recall.top_k),
        budget_tokens: budget.unwrap_or(cfg.recall.budget_tokens),
        include_episodic: include_episodic || cfg.recall.include_episodic,
        include_pending: include_pending || cfg.recall.include_pending,
        trust_policy: if include_untrusted {
            TrustPolicy::Fenced
        } else {
            cfg.recall.trust_policy.clone()
        },
        min_score: min_score.map(|v| v as f32).unwrap_or(cfg.recall.min_score),
        weights: RecallWeights {
            sim: cfg.recall.weights.sim,
            importance: cfg.recall.weights.importance,
            decay: cfg.recall.weights.decay,
        },
        half_life: RecallHalfLives {
            working_hours: cfg.recall.half_life.working_hours,
            episodic_hours: cfg.recall.half_life.episodic_hours,
            semantic_hours: cfg.recall.half_life.semantic_hours,
            procedural_hours: cfg.recall.half_life.procedural_hours,
        },
        budget_split: BudgetSplit {
            working: cfg.recall.budget_split.working,
            episodic: cfg.recall.budget_split.episodic,
            semantic: cfg.recall.budget_split.semantic,
            procedural: cfg.recall.budget_split.procedural,
        },
    };

    let query = RecallQuery::new(text.clone(), &recall_cfg);
    let report = crate::recall::recall(&store, &*embedder, &query).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.no_hit {
        println!(
            "{}",
            crate::recall::render_no_hit(&text, recall_cfg.min_score as f64, report.hits.len())
        );
        return Ok(());
    }

    // Build text map for rendering
    let mut texts = std::collections::HashMap::new();
    for hit in &report.hits {
        if hit.injected() {
            let text = if let Some(row) = store.get_memory(hit.public_id.clone()).await? {
                row.text
            } else {
                String::new()
            };
            texts.insert(hit.public_id.clone(), text);
        }
    }

    let output = crate::recall::render_report(&report, &texts, recall_cfg.min_score as f64);
    println!("{}", output);

    if explain {
        // Print component breakdown for each injected hit
        for hit in &report.hits {
            if hit.injected() {
                let components = crate::recall::why(hit);
                println!("# {} — {}", hit.public_id, components.join(", "));
                println!(
                    "  sim={:.4} bm25_rank={:?} importance={:.4} confidence={:.4} decay={:.4}",
                    hit.components.sim.unwrap_or(0.0),
                    hit.components.bm25_rank,
                    hit.components.importance,
                    hit.components.confidence,
                    hit.components.decay
                );
            }
        }
    }

    Ok(())
}

/// `explain` CLI command — drills into one memory's provenance.
async fn explain_cmd(cfg: &Config, id: String) -> Result<()> {
    let store = crate::storage::StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await?;
    let Some(expl) = crate::recall::explain(&store, &cfg.agent_id, &id).await? else {
        eprintln!("memory {} not found (or belongs to another agent)", id);
        std::process::exit(1);
    };

    println!("id:            {}", expl.public_id);
    println!(
        "source:        {} ({})",
        expl.source_kind,
        expl.source_ref.as_deref().unwrap_or("—")
    );
    println!(
        "supersedes:    {}",
        expl.supersedes.as_deref().unwrap_or("—")
    );
    println!(
        "superseded_by: {}",
        expl.superseded_by.as_deref().unwrap_or("—")
    );
    println!("ref_count:     {}", expl.ref_count);
    println!("links:");
    for (kind, target) in &expl.links {
        println!("  {} -> {}", kind, target);
    }
    println!("recall injections:");
    for (recall_id, rank, score, injected) in &expl.recalls {
        println!(
            "  recall={} rank={} score={:.4} injected={}",
            recall_id, rank, score, injected
        );
    }
    Ok(())
}

/// `eval` CLI command — runs the offline eval suite.
async fn eval_cmd(
    cfg: &Config,
    file: std::path::PathBuf,
    min_precision: f64,
    min_recall: f64,
    min_mrr: f64,
    json: bool,
) -> Result<()> {
    use crate::eval::{CaseResult, Metrics, load_cases};

    let cases = load_cases(&file)?;

    // For each case, we need to create a temp store with the corpus memories
    // This is complex - we'll use the existing approach from tests
    // For now, run eval by loading memories into a fresh store per case

    let mut results = Vec::new();

    for case in &cases {
        // Create a temp store for this case
        let dir = tempfile::tempdir().unwrap();
        let mut case_cfg = cfg.clone();
        case_cfg.db_path = dir.path().join("eval.db");

        let store = crate::storage::StoreHandle::open(&case_cfg, defaults::READ_POOL_SIZE).await?;

        // Build embedder
        let dim = store.embed_dim().await?;
        let embedder = crate::embed::embedder_from_config(&case_cfg.embed, dim);
        store.validate_embed_dim(&*embedder).await?;

        // Insert corpus memories
        for _mem_id in &case.corpus {
            // We'd need the actual memory text - in practice the corpus should contain full memories
            // For now, we'll skip this and note it's a placeholder
            // A proper implementation would have the corpus contain the full memory data
        }

        // Run recall
        let query = crate::recall::RecallQuery::new(case.query.clone(), &case_cfg.recall);
        let report = crate::recall::recall(&store, &*embedder, &query).await?;

        let returned: Vec<String> = report
            .hits
            .iter()
            .filter(|h| h.injected())
            .map(|h| h.public_id.clone())
            .collect();

        results.push(CaseResult {
            returned,
            relevant: case.relevant.clone(),
            forbidden: case.forbidden.clone(),
        });
    }

    let metrics = Metrics::aggregate(&results);

    if json {
        println!("{}", serde_json::to_string_pretty(&metrics)?);
    } else {
        println!("precision: {:.3}", metrics.precision);
        println!("recall:    {:.3}", metrics.recall);
        println!("mrr:       {:.3}", metrics.mrr);
        println!("leaks:     {}", metrics.leaks);
        println!("cases:     {}", metrics.cases);
    }

    metrics
        .check(min_precision, min_recall, min_mrr)
        .map_err(crate::error::Error::InvalidInput)?;

    Ok(())
}

// `defaults` is re-exported for binary consumers; keep the import referenced.
#[allow(unused_imports)]
use defaults as _defaults;
