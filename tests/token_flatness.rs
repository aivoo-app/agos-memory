//! v0.6.0 issue 0004 — injected tokens stay flat as candidate/history size grows.

mod common;

use async_trait::async_trait;

use agos_memory::config::{BudgetSplit, Config, RecallConfig};
use agos_memory::embed::Embedder;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{Clock, SystemClock, TokenCounter, sha256_hex};
use common::{DIM, store};

const QUERY: &str = "quartz";
const ITEM_TOKENS: u64 = 200;
const BUDGET_TOKENS: u64 = 1_500;
const CORPUS_SIZES: [usize; 3] = [32, 128, 512];
// sqlite-vec caps KNN k at 4096; recall requests 4 × top_k.
const MAX_TOP_K: usize = 1_024;

#[derive(Debug)]
struct Sample {
    corpus_size: usize,
    report_tokens: u64,
    ledger_tokens: u64,
    injected_items: u64,
}

struct AxisEmbedder;

#[async_trait]
impl Embedder for AxisEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| e0()).collect())
    }

    fn model(&self) -> &str {
        "axis-flatness-mock"
    }

    fn dim(&self) -> usize {
        DIM
    }
}

fn e0() -> Vec<f32> {
    let mut vector = vec![0.0_f32; DIM];
    vector[0] = 1.0;
    vector
}

fn vector_blob() -> Vec<u8> {
    e0().iter().flat_map(|value| value.to_le_bytes()).collect()
}

/// Build a base text of at least `target` tokens. Replacing its first word with
/// a fixed-width row tag keeps every row's measured cost identical while making
/// every text/hash/public id unique.
fn base_text() -> String {
    let counter = agos_memory::util::tokens::HeuristicCounter::new();
    let mut words = Vec::new();
    let mut index = 0;
    loop {
        let candidate = words.join(" ");
        if counter.count(&candidate) >= ITEM_TOKENS {
            return candidate;
        }
        words.push(format!("b{index}"));
        index += 1;
    }
}

fn fixed_width_tag(index: usize) -> String {
    let mut value = index;
    let mut letters = [b'a'; 3];
    for slot in &mut letters {
        *slot = b'a' + (value % 26) as u8;
        value /= 26;
    }
    String::from_utf8(letters.to_vec()).expect("ASCII tag")
}

fn text_for(base: &str, index: usize) -> String {
    let tag = fixed_width_tag(index);
    format!("r{tag} {}", base.split_once(' ').expect("base words").1)
}

async fn seed_corpus(store: &StoreHandle, corpus_size: usize) -> Result<()> {
    let base = base_text();
    let vector = vector_blob();
    store
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = SystemClock.now_millis();
            for index in 0..corpus_size {
                let text = text_for(&base, index);
                let public_id = format!("flat_{corpus_size}_{index}_{}", &sha256_hex(&text)[..12]);
                let hash = sha256_hex(&text);
                tx.execute(
                    "INSERT INTO memories
                     (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                      status, trust, importance_current, confidence, pinned,
                      expires_at, last_referenced_at, created_at, updated_at)
                     VALUES (?1, 'default', 'semantic', 'fact', ?2, ?3, 'user',
                             'active', 'trusted', 0.5, 1.0, 0,
                             NULL, NULL, ?4, ?4)",
                    rusqlite::params![public_id, text, hash, now],
                )?;
                let row_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO vec_memories
                     (rowid, embedding, tier, status, trust, kind, pinned)
                     VALUES (?1, ?2, 'semantic', 0, 0, 'fact', 0)",
                    rusqlite::params![row_id, vector],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

fn flatness_config(corpus_size: usize) -> RecallConfig {
    RecallConfig {
        top_k: corpus_size.min(MAX_TOP_K),
        budget_tokens: BUDGET_TOKENS,
        min_score: 0.0,
        budget_split: BudgetSplit {
            working: 0.0,
            episodic: 0.0,
            semantic: 1.0,
            procedural: 0.0,
        },
        ..Config::default().recall
    }
}

async fn sample(corpus_size: usize) -> Result<Sample> {
    let (store, _dir) = store(&format!("flat_{corpus_size}.db")).await;
    seed_corpus(&store, corpus_size).await?;

    let config = flatness_config(corpus_size);
    let query = RecallQuery::new(QUERY, &config);
    let report = recall(&store, &AxisEmbedder, &query).await?;

    let injected_items = report.hits.iter().filter(|hit| hit.injected()).count() as u64;
    let report_sum: u64 = report
        .hits
        .iter()
        .filter(|hit| hit.injected())
        .map(|hit| hit.tokens)
        .sum();
    assert_eq!(
        report.tokens_used, report_sum,
        "answer accounting must equal placed hit tokens at corpus {corpus_size}"
    );
    assert!(
        injected_items > 0 && !report.no_hit,
        "flatness must not pass vacuously at corpus {corpus_size}"
    );
    assert!(
        report.tokens_used <= BUDGET_TOKENS,
        "corpus {corpus_size} exceeded budget: {}",
        report.tokens_used
    );

    let ledger_tokens = store
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT tokens_used FROM token_ledger ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|value| value as u64)?)
        })
        .await?;
    assert_eq!(
        ledger_tokens, report.tokens_used,
        "token_ledger and answer accounting disagree at corpus {corpus_size}"
    );

    Ok(Sample {
        corpus_size,
        report_tokens: report.tokens_used,
        ledger_tokens,
        injected_items,
    })
}

#[tokio::test]
async fn injected_tokens_are_flat_across_history_sizes() -> Result<()> {
    let mut samples = Vec::new();
    for corpus_size in CORPUS_SIZES {
        samples.push(sample(corpus_size).await?);
    }

    let counts: Vec<u64> = samples.iter().map(|sample| sample.report_tokens).collect();
    let min = *counts.iter().min().expect("non-empty counts");
    let max = *counts.iter().max().expect("non-empty counts");
    let mean = counts.iter().copied().sum::<u64>() as f64 / counts.len() as f64;
    let range_ratio = (max - min) as f64 / mean;

    println!("token flatness (budget={BUDGET_TOKENS}, item={ITEM_TOKENS}):");
    for sample in &samples {
        println!(
            "  corpus={:>4}: report={} ledger={} injected_items={}",
            sample.corpus_size, sample.report_tokens, sample.ledger_tokens, sample.injected_items
        );
    }
    println!("  min={min} max={max} mean={mean:.2} range_ratio={range_ratio:.4}");

    assert!(
        range_ratio < 0.05,
        "injected-token range ratio {range_ratio:.4} >= 0.05 across {counts:?}"
    );
    Ok(())
}
