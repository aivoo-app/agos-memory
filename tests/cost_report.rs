//! v0.6.0 issue 0003 — operator cost report and per-session ceiling acceptance.

use std::path::Path;
use std::process::Command;

use agos_memory::config::{Config, EmbedConfig, EmbedProvider, LlmConfig};
use agos_memory::embed::HashEmbedder;
use agos_memory::error::Result;
use agos_memory::llm::MockChat;
use agos_memory::memory::extract::extract_session;
use agos_memory::memory::sessions::{append_turn, open_session};
use agos_memory::observe::ledger::{self, Purpose};
use agos_memory::storage::StoreHandle;
use agos_memory::util::Clock;

#[derive(Debug, Default, PartialEq)]
struct RawSessionTotals {
    calls: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cost_micros: i64,
}

fn test_config(db: &Path) -> Config {
    Config {
        db_path: db.to_path_buf(),
        embed: EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        llm: LlmConfig {
            base_url: String::new(),
            ..Default::default()
        },
        budget: agos_memory::config::BudgetConfig {
            max_tokens_per_session: 1,
        },
        ..Default::default()
    }
}

fn run_cli(dir: &Path, config: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agos-memory"))
        .arg("--config")
        .arg(config)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn agos-memory");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn seed_session() -> Result<(
    tempfile::TempDir,
    std::path::PathBuf,
    String,
    RawSessionTotals,
)> {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("cost.db");
    let config_path = dir.path().join("agos-memory.toml");
    let cfg = test_config(&db);
    std::fs::write(
        &config_path,
        format!(
            "db_path = '{}'\nagent_id = 'default'\n\n\
             [embed]\nprovider = 'hash'\n\n\
             [llm]\nbase_url = ''\n\n\
             [budget]\nmax_tokens_per_session = 1\n",
            db.display()
        ),
    )
    .expect("write config");

    let store = StoreHandle::open(&cfg, 2).await?;
    let session = open_session(&store, "default").await?;
    append_turn(
        &store,
        session.id,
        "user",
        "The project codename is Northstar and the launch date is May 12.",
    )
    .await?;

    // A deterministic mock extraction exercises the real session-attributed
    // ledger path. Hash embedding is also recorded, but as a separate
    // sessionless zero-cost row.
    let chat = MockChat::fixed(
        r#"{"memories":[{"tier":"semantic","kind":"fact","text":"The project codename is Northstar and it launches May 12","importance":0.8,"confidence":0.9,"session_independent":true}]}"#,
    );
    let dim = store.embed_dim().await?;
    extract_session(&store, session.id, &chat, &HashEmbedder::new(dim), None).await?;

    let summarize = ledger::estimated_entry_for_session(
        Purpose::Summarize,
        Some(session.id),
        "test-price-model",
        "summarize the project fact",
        "Northstar launches May 12",
        25,
        true,
    );
    ledger::record_entry(&store, &summarize).await?;

    store
        .write(move |conn| {
            let now = agos_memory::util::SystemClock.now_millis();
            conn.execute(
                "INSERT INTO token_ledger
                 (session_id, budget, tokens_used, tier_split_json, items_injected,
                  items_dropped, created_at)
                 VALUES (?1, 1500, 80, '{}', 2, 1, ?2),
                        (?1, 1500, 40, '{}', 1, 2, ?2)",
                rusqlite::params![session.id, now],
            )?;
            Ok(())
        })
        .await?;

    let raw = store
        .read(move |conn| {
            let totals = conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(prompt_tokens), 0),
                        COALESCE(SUM(completion_tokens), 0),
                        COALESCE(SUM(total_tokens), 0),
                        COALESCE(CAST(ROUND(SUM(cost_usd_est) * 1000000) AS INTEGER), 0)
                 FROM llm_calls WHERE session_id = ?1",
                [session.id],
                |row| {
                    Ok(RawSessionTotals {
                        calls: row.get(0)?,
                        prompt_tokens: row.get(1)?,
                        completion_tokens: row.get(2)?,
                        total_tokens: row.get(3)?,
                        cost_micros: row.get(4)?,
                    })
                },
            )?;
            Ok(totals)
        })
        .await?;

    // The ceiling is enforced against this session, not the global ledger.
    let err = agos_memory::memory::check_session_budget(&store, &cfg, Some(session.id))
        .await
        .expect_err("one-token ceiling must be reached");
    assert!(err.to_string().contains("session token ceiling reached"));

    drop(store);
    Ok((dir, config_path, session.public_id, raw))
}

#[test]
fn cli_cost_report_matches_ledger_and_token_budget_rows() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let (dir, config, session_id, raw) = runtime.block_on(seed_session())?;
    let dir_path = dir.path().to_path_buf();

    let (code, stdout, stderr) = run_cli(
        &dir_path,
        &config,
        &["cost", "--session", &session_id, "--since", "30m", "--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout)?;
    assert_eq!(json["calls"], raw.calls);
    assert_eq!(json["prompt_tokens"], raw.prompt_tokens);
    assert_eq!(json["completion_tokens"], raw.completion_tokens);
    assert_eq!(json["total_tokens"], raw.total_tokens);
    assert_eq!(
        (json["cost_usd_est"].as_f64().unwrap() * 1_000_000.0).round() as i64,
        raw.cost_micros
    );
    assert_eq!(json["session_budget"]["ceiling_tokens"], 1);
    assert_eq!(json["session_budget"]["used_tokens"], raw.total_tokens);
    assert_eq!(json["session_budget"]["remaining_tokens"], 0);
    assert_eq!(json["session_budget"]["over_budget"], true);
    assert_eq!(json["recall_budget"]["rows"], 2);
    assert_eq!(json["recall_budget"]["budget_tokens_total"], 3000);
    assert_eq!(json["recall_budget"]["tokens_used_total"], 120);
    assert_eq!(json["recall_budget"]["items_injected_total"], 3);
    assert_eq!(json["recall_budget"]["items_dropped_total"], 3);

    let purposes = json["by_purpose"].as_array().expect("by_purpose array");
    assert!(purposes.iter().any(|row| row["purpose"] == "extract"));
    assert!(purposes.iter().any(|row| row["purpose"] == "summarize"));
    assert!(
        purposes.iter().all(|row| row["purpose"] != "embed"),
        "sessionless hash embedding must not be misattributed"
    );

    let (code, human, stderr) = run_cli(
        &dir_path,
        &config,
        &["cost", "--session", &session_id, "--since", "30m"],
    );
    assert_eq!(code, 0, "{human}{stderr}");
    assert!(human.contains(&format!("scope:      session {session_id}")));
    assert!(human.contains(&format!("calls:      {}", raw.calls)));
    assert!(human.contains(&format!("tokens:     {}", raw.total_tokens)));
    assert!(human.contains("recall:     rows=2 budget=3000 used=120"));
    assert!(human.contains("OVER BUDGET"));
    assert!(human.contains("estimated at $0.60/1M tokens"));

    let (code, _, stderr) = run_cli(
        &dir_path,
        &config,
        &["cost", "--session", "missing-session", "--json"],
    );
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("unknown session missing-session"),
        "{stderr}"
    );
    Ok(())
}
