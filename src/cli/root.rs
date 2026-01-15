//! Root clap definition and command dispatch.

use clap::{Parser, Subcommand};
use rusqlite::Connection;

use crate::config::{
    BudgetSplit, Config, RecallConfig, RecallHalfLives, RecallWeights, TrustPolicy,
};
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
    /// Manage memory lifecycle: deprecate, restore, purge, audit.
    Forget {
        #[command(subcommand)]
        cmd: ForgetCmd,
    },
    /// Summarize memories (on-demand).
    Summarize {
        /// Summarize a single memory by public id.
        #[arg(long)]
        id: Option<String>,
        /// Summarize all memories in a tier.
        #[arg(long)]
        tier: Option<String>,
        /// Summarize all memories without a summary.
        #[arg(long)]
        all: bool,
        /// Overwrite existing summaries.
        #[arg(long)]
        force: bool,
    },
    /// Run maintenance jobs (TTL reaper, consolidation).
    Maintain {
        /// Run the TTL reaper now.
        #[arg(long)]
        ttl: bool,
        /// Run consolidation now.
        #[arg(long)]
        consolidate: bool,
        /// Run the reaper/consolidation scheduler in the foreground, honoring
        /// `[memory] reaper_hour` and `[consolidate]` from config.
        #[arg(long)]
        schedule: bool,
    },
}

/// Forget subcommands.
#[derive(Debug, Subcommand)]
pub enum ForgetCmd {
    /// Soft-deprecate a memory (restorable).
    Soft {
        /// Public id of the memory to deprecate.
        id: String,
        /// Reason for deprecation.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Restore a soft-deprecated memory.
    Restore {
        /// Public id of the memory to restore.
        id: String,
    },
    /// Hard-purge a memory (irreversible).
    Hard {
        /// Public id of the memory to purge.
        id: String,
        /// Reason for purge.
        #[arg(long)]
        reason: Option<String>,
    },
    /// List forget audit entries.
    ListAudit {
        /// Filter by action (deprecate, restore, hard_delete, ttl_deprecate, ttl_purge).
        #[arg(long)]
        action: Option<String>,
        /// Only show entries since this timestamp (epoch millis).
        #[arg(long)]
        since: Option<i64>,
        /// Limit number of results.
        #[arg(long)]
        limit: Option<i64>,
    },
    /// List tombstones (hard-purged memories).
    ListTombstones,
    /// Rollback a memory to a prior version (writes a new head; chain intact).
    Rollback {
        /// Public id of the memory to roll back.
        id: String,
        /// Target version number to restore (`--to-version`).
        #[arg(long)]
        to_version: i64,
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
        Command::Forget { cmd } => forget_cmd(&cfg, cmd).await,
        Command::Summarize {
            id,
            tier,
            all,
            force,
        } => summarize_cmd(&cfg, id, tier, all, force).await,
        Command::Maintain {
            ttl,
            consolidate,
            schedule,
        } => maintain_cmd(&cfg, ttl, consolidate, schedule).await,
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
#[allow(clippy::too_many_arguments)]
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

/// `eval` CLI command — runs the offline eval suite end-to-end.
///
/// Each case gets a fresh database; its corpus is seeded through the real
/// write path (embed + dedup + version + vector row) and scored with the real
/// recall path (issue 0052 — no stub, no id-only corpus). Failing cases are
/// printed so a threshold miss is actionable.
async fn eval_cmd(
    cfg: &Config,
    file: std::path::PathBuf,
    min_precision: f64,
    min_recall: f64,
    min_mrr: f64,
    json: bool,
) -> Result<()> {
    use crate::eval::{load_cases, run, validate_cases};

    let cases = load_cases(&file)?;
    validate_cases(&cases)?;

    let eval = run(cfg, &cases).await?;
    let metrics = &eval.metrics;

    if json {
        println!("{}", serde_json::to_string_pretty(metrics)?);
    } else {
        println!("precision: {:.3}", metrics.precision);
        println!("recall:    {:.3}", metrics.recall);
        println!("mrr:       {:.3}", metrics.mrr);
        println!("leaks:     {}", metrics.leaks);
        println!("cases:     {}", metrics.cases);
    }

    if let Err(reason) = metrics.check(min_precision, min_recall, min_mrr) {
        for outcome in eval.imperfect() {
            eprintln!(
                "case {}: query={:?} missed={:?} leaked={:?} returned={:?} relevant={:?}",
                outcome.id,
                outcome.query,
                outcome.missed,
                outcome.leaked,
                outcome.returned,
                outcome.relevant
            );
        }
        return Err(crate::error::Error::InvalidInput(reason));
    }

    Ok(())
}

// `defaults` is re-exported for binary consumers; keep the import referenced.
#[allow(unused_imports)]
use defaults as _defaults;

/// `forget` CLI command — manage memory lifecycle.
async fn forget_cmd(cfg: &Config, cmd: ForgetCmd) -> Result<()> {
    let store = crate::storage::StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await?;
    match cmd {
        ForgetCmd::Soft { id, reason } => {
            let row = store.get_memory(id.clone()).await?.ok_or_else(|| {
                crate::error::Error::InvalidInput(format!("memory {id} not found"))
            })?;
            store.deprecate_memory(row.id, Some("agent")).await?;
            println!("deprecated: {} ({})", row.public_id, row.status);
            if let Some(r) = reason {
                println!("reason: {r}");
            }
        }
        ForgetCmd::Restore { id } => {
            let row = store.get_memory(id.clone()).await?.ok_or_else(|| {
                crate::error::Error::InvalidInput(format!("memory {id} not found"))
            })?;
            store.restore_memory(row.id, Some("agent")).await?;
            println!("restored: {} ({})", row.public_id, row.status);
        }
        ForgetCmd::Hard { id, reason } => {
            let row = store.get_memory(id.clone()).await?.ok_or_else(|| {
                crate::error::Error::InvalidInput(format!("memory {id} not found"))
            })?;
            store
                .hard_purge_memory(row.id, Some("agent"), reason.as_deref())
                .await?;
            println!("hard-purged: {} (tombstone written)", row.public_id);
        }
        ForgetCmd::ListAudit {
            action,
            since,
            limit,
        } => {
            let entries = store.list_forget_audit(action, since, limit).await?;
            println!("forget_audit entries: {}", entries.len());
            for e in &entries {
                println!(
                    "  [{}] {} by {} at {}{}",
                    e.id,
                    e.action,
                    e.requester,
                    e.created_at,
                    e.reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                );
            }
        }
        ForgetCmd::Rollback { id, to_version } => {
            let row = store.get_memory(id.clone()).await?.ok_or_else(|| {
                crate::error::Error::InvalidInput(format!("memory {id} not found"))
            })?;
            if to_version < 1 {
                return Err(crate::error::Error::InvalidInput(
                    "to-version must be >= 1".into(),
                ));
            }
            let rolled = store
                .rollback_memory(row.id, to_version, Some("agent"))
                .await?;
            println!(
                "rolled back: {} to v{} ({})",
                rolled.public_id, to_version, rolled.status
            );
        }
        ForgetCmd::ListTombstones => {
            let tombstones = store.list_tombstones().await?;
            println!("tombstones: {}", tombstones.len());
            for t in &tombstones {
                println!(
                    "  [{}] {} (deleted_at={}, by={:?})",
                    t.id, t.public_id, t.deleted_at, t.deleted_by
                );
            }
        }
    }
    Ok(())
}

/// `summarize` CLI command — on-demand summarization.
///
/// 0056: the LLM is built from config (`chat_from_config`), so a configured
/// provider is used; offline (empty `base_url`) falls back to `MockChat`,
/// mirroring `maintain_cmd`.
async fn summarize_cmd(
    cfg: &Config,
    id: Option<String>,
    tier: Option<String>,
    all: bool,
    force: bool,
) -> Result<()> {
    use crate::memory::summarize_by_id;
    use crate::memory::summarize_tier;

    let store = crate::storage::StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await?;
    let llm = crate::llm::chat_from_config(&cfg.llm, None);

    if let Some(pid) = id {
        let row = store
            .get_memory(pid.clone())
            .await?
            .ok_or_else(|| crate::error::Error::InvalidInput(format!("memory {pid} not found")))?;
        let report = summarize_by_id(&store, &llm, cfg, row.id, force).await?;
        println!(
            "summarized: {} ({} tokens)",
            report.memory.public_id, report.summary_tokens
        );
        println!("summary: {}", report.summary_text);
    } else if let Some(t) = tier {
        let reports = summarize_tier(&store, &llm, cfg, &t).await?;
        println!("summarized {} memories in tier '{}'", reports.len(), t);
        for r in &reports {
            println!("  {} ({} tokens)", r.memory.public_id, r.summary_tokens);
        }
    } else if all {
        // Summarize all tiers
        let mut total = 0;
        for tier in ["working", "episodic", "semantic", "procedural"] {
            let reports = summarize_tier(&store, &llm, cfg, tier).await?;
            total += reports.len();
            for r in &reports {
                println!(
                    "  [{}] {} ({} tokens)",
                    tier, r.memory.public_id, r.summary_tokens
                );
            }
        }
        println!("summarized {} memories total", total);
    } else {
        return Err(crate::error::Error::InvalidInput(
            "specify --id, --tier, or --all".into(),
        ));
    }
    Ok(())
}

/// `maintain` CLI command — run maintenance jobs, or the scheduler.
///
/// 0055: the LLM is built from config (`chat_from_config`), so a configured
/// provider is used; offline (empty `base_url`) falls back to `MockChat`.
/// `--schedule` runs the foreground scheduler that honors `[memory]
/// reaper_hour` and `[consolidate]`, enqueuing `maintain` jobs for the worker.
async fn maintain_cmd(cfg: &Config, ttl: bool, consolidate: bool, schedule: bool) -> Result<()> {
    let store = crate::storage::StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await?;
    let llm = crate::llm::chat_from_config(&cfg.llm, None);

    if schedule {
        return run_scheduler(&store, cfg).await;
    }

    if !ttl && !consolidate {
        return Err(crate::error::Error::InvalidInput(
            "specify --ttl and/or --consolidate (or --schedule)".into(),
        ));
    }

    if ttl {
        let report = crate::memory::run_ttl_reaper(&store, cfg).await?;
        println!(
            "TTL reaper: {} soft-deprecated, {} hard-purged",
            report.soft_deprecated, report.hard_purged
        );
    }

    if consolidate {
        let report = crate::memory::run_consolidation_job(&store, &llm, cfg).await?;
        println!(
            "Consolidation: {} summaries, {} dedup merges, {} orphans deleted",
            report.summaries_generated, report.dedup_clusters_merged, report.orphans_deleted
        );
    }

    Ok(())
}

/// What the scheduler should enqueue at one tick, derived from the clock
/// and config. Pure (no I/O) so it is unit-testable without a database.
///
/// `now` is minutes since the Unix epoch; the caller converts wall-clock to
/// that form. Returns the action strings to enqueue, in declared order.
/// Day-of-week of the Unix epoch (1970-01-01) in the 0=Sunday convention:
/// Thursday = 4. `now_minutes` is minutes since that epoch, so the weekday
/// is `(4 + now_minutes / 1440) % 7`.
const EPOCH_WEEKDAY: u32 = 4;

pub fn scheduler_tick(now_minutes: u64, cfg: &Config) -> Vec<&'static str> {
    let hour = (now_minutes / 60) as u32 % 24;
    let minute = (now_minutes % 60) as u32;
    let mut out = Vec::new();

    if cfg.memory.reaper_hour() != u32::MAX && hour == cfg.memory.reaper_hour() && minute == 0 {
        out.push("ttl");
    }
    if cfg.consolidate.enabled {
        let day = (EPOCH_WEEKDAY + (now_minutes / (24 * 60)) as u32) % 7;
        if day == cfg.consolidate.day && hour == cfg.consolidate.hour && minute == 0 {
            out.push("consolidate");
        }
    }
    out
}

/// Foreground scheduler: enqueues `maintain` jobs at the configured hours.
///
/// Honors `[memory] reaper_hour` (TTL reaper) and `[consolidate]`
/// (`enabled`/`day`/`hour` for the consolidation pass) so the config knobs are
/// no longer dead (0055). Runs until the process is signalled.
async fn run_scheduler(store: &crate::storage::StoreHandle, cfg: &Config) -> Result<()> {
    use crate::memory::jobs;
    use crate::util::clock::Clock;

    eprintln!(
        "scheduler: reaper_hour={}, consolidate.enabled={}, consolidate.hour={}",
        cfg.memory.reaper_hour(),
        cfg.consolidate.enabled,
        cfg.consolidate.hour
    );

    let tick = std::time::Duration::from_secs(60);
    let clock = crate::util::SystemClock;
    loop {
        let now = clock.now_millis();
        let now_minutes = (now / 60_000) as u64;

        // `enqueue` is idempotent on the idempotency key (`scheduler:ttl`,
        // `scheduler:consolidate`), so re-ticking the same minute is a no-op.
        for action in scheduler_tick(now_minutes, cfg) {
            let payload = serde_json::json!({"action": action}).to_string();
            let id_key = format!("scheduler:{action}");
            if let Err(e) = jobs::enqueue(store, "maintain", &payload, Some(&id_key)).await {
                eprintln!("scheduler: enqueue {action} failed: {e}");
            }
        }

        tokio::time::sleep(tick).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `now_minutes` is minutes since the Unix epoch; the caller converts
    /// wall-clock to that form. The epoch is a Thursday, so day 0 = Thursday
    /// (day 4 of the 0=Sunday week).
    const EPOCH_MINUTES: u64 = 0; // 1970-01-01 00:00 UTC = Thursday 00:00

    fn cfg(reaper_hour: u32, cons_enabled: bool, cons_day: u32, cons_hour: u32) -> Config {
        Config {
            memory: crate::config::MemoryConfig {
                reaper_hour,
                ..Default::default()
            },
            consolidate: crate::config::ConsolidateConfig {
                enabled: cons_enabled,
                day: cons_day,
                hour: cons_hour,
            },
            ..Config::default()
        }
    }

    #[test]
    fn scheduler_tick_silent_off_hour() {
        // 02:30 — neither the reaper (03:00) nor consolidation (Sun 04:00).
        let c = cfg(3, true, 0, 4);
        let tick = EPOCH_MINUTES + 2 * 60 + 30;
        assert!(
            scheduler_tick(tick, &c).is_empty(),
            "off-hour must enqueue nothing"
        );
    }

    #[test]
    fn scheduler_tick_fires_reaper_at_the_hour() {
        let c = cfg(3, true, 0, 4);
        let tick = EPOCH_MINUTES + 3 * 60; // 03:00
        assert_eq!(scheduler_tick(tick, &c), vec!["ttl"]);
    }

    #[test]
    fn scheduler_tick_fires_consolidation_at_its_hour() {
        // Epoch is Thursday; day 0 (Sunday) is 3 days later.
        let c = cfg(3, true, 0, 4);
        let tick = EPOCH_MINUTES + (3 * 24 + 4) * 60; // Sunday 04:00
        assert_eq!(scheduler_tick(tick, &c), vec!["consolidate"]);
    }

    #[test]
    fn scheduler_tick_fires_both_when_hours_coincide() {
        let c = cfg(4, true, 0, 4);
        let tick = EPOCH_MINUTES + (3 * 24 + 4) * 60; // Sunday 04:00
        assert_eq!(scheduler_tick(tick, &c), vec!["ttl", "consolidate"]);
    }

    #[test]
    fn scheduler_tick_skips_consolidation_when_disabled() {
        let c = cfg(4, false, 0, 4);
        let tick = EPOCH_MINUTES + (3 * 24 + 4) * 60;
        assert_eq!(scheduler_tick(tick, &c), vec!["ttl"]);
    }

    #[test]
    fn scheduler_tick_skips_reaper_when_hour_is_max() {
        // `u32::MAX` = disabled (the default when the knob is absent).
        let mut c = Config::default();
        c.memory.reaper_hour = u32::MAX;
        let tick = EPOCH_MINUTES + 4 * 60;
        assert!(scheduler_tick(tick, &c).is_empty());
    }

    #[test]
    fn scheduler_tick_requires_minute_zero() {
        // Same hour, minute 30 — must not fire.
        let c = cfg(3, false, 0, 0);
        let tick = EPOCH_MINUTES + 3 * 60 + 30;
        assert!(scheduler_tick(tick, &c).is_empty());
    }
}
