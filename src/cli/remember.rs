//! `remember` / `session` commands — the CLI face of the write path (0030).

use crate::config::{Config, EmbedProvider};
use crate::embed::OpenAiCompatEmbedder;
use crate::error::{Error, Result};
use crate::http::HttpConfig;
use crate::memory::{self, jobs, sessions};
use crate::storage::StoreHandle;
use crate::{defaults, observe};

/// Open the store from CLI config (shared helper; also used by the MCP server).
pub async fn open_store(cfg: &Config) -> Result<StoreHandle> {
    StoreHandle::open(cfg, defaults::READ_POOL_SIZE).await
}

/// Enqueue an `extract` job for a session (best-effort — never fails the
/// caller). The worker picks it up later and runs the real extractor.
async fn enqueue_extract(store: &StoreHandle, session_id: i64) {
    let payload = serde_json::json!({"session": session_id}).to_string();
    if let Err(e) = jobs::enqueue(store, "extract", &payload, None).await {
        eprintln!("warning: failed to enqueue extract job for session {session_id}: {e}");
    }
}

/// `remember --text …`: one fact in, one memory out.
pub async fn run_remember(
    cfg: &Config,
    text: &str,
    tier: &str,
    kind: &str,
    source_kind: &str,
    confidence: f64,
) -> Result<()> {
    let store = open_store(cfg).await?;
    check_budget(&store, cfg).await?;
    let row = match cfg.embed.provider {
        // Degraded: keyword-only recall, no vectors to compare (D4).
        EmbedProvider::None => {
            memory::remember_degraded(
                &store,
                tier,
                kind,
                text,
                source_kind,
                confidence,
                observe::EXTRACTOR_VERSION,
                cfg.memory.pending_threshold,
            )
            .await?
        }
        EmbedProvider::Hash => {
            // Deterministic offline embedder (evals, benchmarks, local dev):
            // same hybrid path as production, no provider involved.
            let dim = store.embed_dim().await?;
            let embedder = crate::embed::HashEmbedder::new(dim);
            store.validate_embed_dim(&embedder).await?;
            memory::remember(
                &store,
                tier,
                kind,
                text,
                source_kind,
                confidence,
                &embedder,
                observe::EXTRACTOR_VERSION,
                cfg.memory.pending_threshold,
                cfg.memory.dedup_threshold,
            )
            .await?
        }
        EmbedProvider::OpenAiCompat => {
            if cfg.embed.model.is_empty() {
                return Err(Error::Config(
                    "embed.model is empty; set [embed] model or use provider = 'none'".into(),
                ));
            }
            let http = HttpConfig::new(
                cfg.embed.base_url.clone(),
                cfg.embed.api_key.clone(),
                cfg.embed.timeout_secs,
            );
            // Dim comes from the database (pinned in `meta` at init).
            let dim = store.embed_dim().await?;
            let embedder = OpenAiCompatEmbedder::new(http, cfg.embed.model.clone(), dim);
            store.validate_embed_dim(&embedder).await?;
            memory::remember(
                &store,
                tier,
                kind,
                text,
                source_kind,
                confidence,
                &embedder,
                observe::EXTRACTOR_VERSION,
                cfg.memory.pending_threshold,
                cfg.memory.dedup_threshold,
            )
            .await?
        }
    };
    println!("memory:  {} ({})", row.public_id, row.status);
    println!("trust:   {}", row.trust);
    println!("tier:    {} kind: {}", row.tier, row.kind);
    Ok(())
}

/// Refuse extraction past the per-session token ceiling (degraded: turns still logged).
async fn check_budget(store: &StoreHandle, cfg: &Config) -> Result<()> {
    memory::check_open_session_budget(store, cfg).await
}

/// `session open|append|close|idle-close`.
pub async fn run_session(cfg: &Config, cmd: &super::root::SessionCmd) -> Result<()> {
    use super::root::SessionCmd;
    let store = open_store(cfg).await?;
    match cmd {
        SessionCmd::Open => {
            let s = sessions::open_session(&store, &cfg.agent_id).await?;
            println!("session: {} (open)", s.public_id);
            Ok(())
        }
        SessionCmd::Append {
            session,
            role,
            content,
        } => {
            let public_id = session.clone().unwrap_or_default();
            let id = if public_id.is_empty() {
                sessions::get_open_session(&store, &cfg.agent_id)
                    .await?
                    .map(|s| s.id)
                    .ok_or_else(|| {
                        Error::InvalidInput(
                            "no open session for this agent — run `session open` first".into(),
                        )
                    })?
            } else {
                sessions::session_id_by_public(&store, &public_id)
                    .await?
                    .ok_or_else(|| Error::InvalidInput(format!("unknown session {public_id}")))?
            };
            let t = sessions::append_turn(&store, id, role.as_str(), content.as_str()).await?;
            println!("turn:    #{} ({role})", t.seq);
            Ok(())
        }
        SessionCmd::Close { session } => {
            let id = match session {
                Some(public_id) => sessions::session_id_by_public(&store, public_id)
                    .await?
                    .ok_or_else(|| Error::InvalidInput(format!("unknown session {public_id}")))?,
                None => sessions::get_open_session(&store, &cfg.agent_id)
                    .await?
                    .map(|s| s.id)
                    .ok_or_else(|| Error::InvalidInput("no open session".into()))?,
            };
            sessions::close_session(&store, id, "explicit").await?;
            // 0055: enqueue extraction so the worker runs the extractor on the
            // just-closed session's turns. Best-effort: the close is the user's
            // action and must succeed even if the queue is misbehaving.
            enqueue_extract(&store, id).await;
            println!("session closed");
            Ok(())
        }
        SessionCmd::IdleClose => {
            let closed = sessions::close_idle_sessions(&store, cfg.session.idle_minutes).await?;
            // 0055: enqueue extraction for every idle-closed session.
            for s in &closed {
                enqueue_extract(&store, *s).await;
            }
            println!("idle-closed: {}", closed.len());
            Ok(())
        }
    }
}
