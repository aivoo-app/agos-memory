//! Hard filter — the single source of truth for "what may reach scoring".
//!
//! Decisions D6/D29: filters run *before* scoring and *inside* both retrieval
//! legs, and the predicate is re-applied *after* fusion. Nothing with a foreign
//! `agent_id`, a non-eligible `tier`/`status`/`trust`, an expired `expires_at`
//! or a superseding successor may ever reach scoring, on any path.
//!
//! The predicate necessarily exists in two forms:
//!
//! 1. **SQL** ([`HardFilter::predicate`]) — evaluated by SQLite, authoritative
//!    for set membership in the FTS leg and in both post-leg lookups.
//! 2. **Rust** ([`HardFilter::admits`]) — the mirror used to gate resolved
//!    rows, so a stale vec0 metadata row can never smuggle itself past the
//!    canonical row's veto.
//!
//! Both derive from the same fields in the same order, and they are applied as
//! an *intersection* (fail closed: a row must satisfy both to survive).
//! `tests/recall_filters.rs` proves them equivalent row-for-row, so they cannot
//! drift apart unnoticed.
//!
//! Three nuances, each easy to get backwards:
//!
//! - **Supersede direction.** A *replacement* row carries `supersedes_id`
//!   pointing at the row it replaces; the *replaced* row carries
//!   `superseded_by_id`. The old row is therefore excluded by
//!   `superseded_by_id IS NULL` — filtering on `supersedes_id IS NULL` would
//!   keep the stale row and drop its replacement. As extra hardening we also
//!   exclude anything *pointed at* by another row's `supersedes_id`, so a
//!   writer that forgets to back-fill `superseded_by_id` still cannot leak the
//!   replaced row.
//! - **`last_referenced_at` sanity.** That clause is NULL-safe; a naive
//!   `last_referenced_at >= created_at` evaluates to NULL (hence false) for
//!   every never-referenced row and would hide nearly the whole store.
//! - **Pin does not launder provenance.** A pinned untrusted row is still
//!   excluded under `TrustPolicy::Strict`; `pinned` only affects ordering and
//!   packing (0034).

use std::collections::{HashMap, HashSet};

use crate::config::TrustPolicy;
use crate::error::Result;
use crate::storage::StoreHandle;

use super::query::RecallQuery;

/// Vec0 metadata code for `status` (CHECK order in the `memories` schema).
const STATUS_ACTIVE: i64 = 0;
const STATUS_PENDING: i64 = 1;

/// Vec0 metadata code for `trust` (CHECK order in the `memories` schema).
const TRUST_TRUSTED: i64 = 0;
const TRUST_UNTRUSTED: i64 = 1;
const TRUST_SYSTEM: i64 = 2;

/// SQL string literal with embedded quotes doubled.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Comma-joined SQL literal list, e.g. `('working','semantic')`.
fn quote_list(items: &[&str]) -> String {
    items.iter().map(|s| quote(s)).collect::<Vec<_>>().join(",")
}

/// Integer `status` code used by the `vec_memories` metadata column.
fn status_code(status: &str) -> i64 {
    match status {
        "active" => STATUS_ACTIVE,
        "pending" => STATUS_PENDING,
        "deprecated" => 2,
        "deleted" => 3,
        other => unreachable!("unknown status {other} in allowlist"),
    }
}

/// Integer `trust` code used by the `vec_memories` metadata column.
fn trust_code(trust: &str) -> i64 {
    match trust {
        "trusted" => TRUST_TRUSTED,
        "untrusted" => TRUST_UNTRUSTED,
        "system" => TRUST_SYSTEM,
        other => unreachable!("unknown trust {other} in allowlist"),
    }
}

/// A resolved `memories` row plus the two DB-derived supersede signals.
///
/// Every field the predicate reads is materialised here, so the Rust mirror
/// [`HardFilter::admits`] needs no further queries. The struct also carries what
/// the *scoring* stage (0033) and the *packing* stage (0034) need — rerank's
/// `importance_current`/`confidence`, and packing's `pinned`/`text`/`summary_*` —
/// so the whole pipeline reads one row per candidate instead of re-querying.
/// Text is loaded for at most the candidate pool (`top_k × 4`), never the store.
///
/// Not `Eq`: `importance_current`/`confidence` are floats (and only `PartialEq`
/// is ever needed — these rows are compared for test assertions, never hashed).
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRow {
    /// `memories.id` (equals the `vec_memories` rowid).
    pub rowid: i64,
    /// External public id.
    pub public_id: String,
    /// Owning agent.
    pub agent_id: String,
    /// Tier of the row.
    pub tier: String,
    /// Lifecycle status.
    pub status: String,
    /// Provenance trust.
    pub trust: String,
    /// Absolute expiry (millis), `None` = never expires.
    pub expires_at: Option<i64>,
    /// The row that replaced this one, when it has been replaced.
    pub superseded_by_id: Option<i64>,
    /// A row whose `supersedes_id` points *at* this one (hardening signal).
    pub successor_id: Option<i64>,
    /// Creation time (millis).
    pub created_at: i64,
    /// Last reference time (millis), `None` = never referenced.
    pub last_referenced_at: Option<i64>,
    /// Current importance in `[0,1]` — the rerank importance term (D23).
    pub importance_current: f64,
    /// Extraction confidence in `[0,1]` — multiplies importance for `pending`
    /// items only (D25/D28).
    pub confidence: f64,
    /// Pinned rows are injected before anything else and are not bound by their
    /// tier's share (D25).
    pub pinned: bool,
    /// Full memory text — what packing places (D25).
    pub text: String,
    /// Pre-computed summary text, the summary-swap fallback when the full text
    /// does not fit (D25).
    pub summary_text: Option<String>,
    /// Token count of [`Self::summary_text`] as recorded by the writer; `None`
    /// or `<= 0` means no summary was counted.
    pub summary_tokens: Option<i64>,
}

/// Columns [`CanonicalRow`] is built from, prefixed for alias `m`. The
/// correlated subquery materialises the supersede hardening signal, so the
/// Rust mirror sees exactly what SQL sees.
pub(super) const CANONICAL_COLUMNS: &str = "m.id, m.public_id, m.agent_id, m.tier, m.status, \
     m.trust, m.expires_at, m.superseded_by_id, \
     (SELECT s.id FROM memories s WHERE s.supersedes_id = m.id LIMIT 1), \
     m.created_at, m.last_referenced_at, m.importance_current, m.confidence, \
     m.pinned, m.text, m.summary_text, m.summary_tokens";

/// Map one [`CANONICAL_COLUMNS`] row, in order.
pub(super) fn row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<CanonicalRow> {
    Ok(CanonicalRow {
        rowid: r.get(0)?,
        public_id: r.get(1)?,
        agent_id: r.get(2)?,
        tier: r.get(3)?,
        status: r.get(4)?,
        trust: r.get(5)?,
        expires_at: r.get(6)?,
        superseded_by_id: r.get(7)?,
        successor_id: r.get(8)?,
        created_at: r.get(9)?,
        last_referenced_at: r.get(10)?,
        importance_current: r.get(11)?,
        confidence: r.get(12)?,
        pinned: r.get(13)?,
        text: r.get(14)?,
        summary_text: r.get(15)?,
        summary_tokens: r.get(16)?,
    })
}

/// Eligible tiers for a query (D26: `episodic` is opt-in).
pub(crate) fn eligible_tiers(include_episodic: bool) -> &'static [&'static str] {
    if include_episodic {
        &["working", "episodic", "semantic", "procedural"]
    } else {
        &["working", "semantic", "procedural"]
    }
}

/// Eligible statuses for a query (D28: `pending` is opt-in).
pub(crate) fn eligible_statuses(include_pending: bool) -> &'static [&'static str] {
    if include_pending {
        &["active", "pending"]
    } else {
        &["active"]
    }
}

/// Eligible trusts for a query (D29: `untrusted` only under [`TrustPolicy::Fenced`]).
pub(crate) fn eligible_trusts(policy: &TrustPolicy) -> &'static [&'static str] {
    match policy {
        TrustPolicy::Strict => &["trusted", "system"],
        TrustPolicy::Fenced => &["trusted", "system", "untrusted"],
    }
}

/// An immutable, fully-resolved hard filter for one recall call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardFilter {
    /// Agent the query is scoped to.
    agent_id: String,
    /// Eligible tiers (D26: episodic opt-in).
    tiers: &'static [&'static str],
    /// Eligible statuses (D28: `pending` opt-in).
    statuses: &'static [&'static str],
    /// Eligible trusts (D29: `untrusted` only under `Fenced`).
    trusts: &'static [&'static str],
    /// Reference time for expiry (millis).
    now: i64,
}

impl HardFilter {
    /// Build the filter a query implies, evaluated at `now`.
    pub fn from_query(agent_id: impl Into<String>, q: &RecallQuery, now: i64) -> Self {
        Self {
            agent_id: agent_id.into(),
            tiers: eligible_tiers(q.include_episodic),
            statuses: eligible_statuses(q.include_pending),
            trusts: eligible_trusts(&q.trust_policy),
            now,
        }
    }

    /// Reference instant this filter was evaluated at (millis).
    ///
    /// Rerank (0033) threads this into the decay term so the instant used for
    /// expiry and the instant used for decay are provably the same one.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// The canonical predicate over alias `alias` of the `memories` table.
    ///
    /// This is *the* definition of hard-filter membership: it is inlined into
    /// the FTS leg and both post-leg lookups.
    pub fn predicate(&self, alias: &str) -> String {
        format!(
            "{a}.agent_id = {agent}
             AND {a}.tier IN ({tiers})
             AND {a}.status IN ({statuses})
             AND {a}.trust IN ({trusts})
             AND ({a}.expires_at IS NULL OR {a}.expires_at > {now})
             AND {a}.superseded_by_id IS NULL
             AND NOT EXISTS (SELECT 1 FROM memories s WHERE s.supersedes_id = {a}.id)
             AND ({a}.last_referenced_at IS NULL
                  OR {a}.last_referenced_at >= {a}.created_at)",
            a = alias,
            agent = quote(&self.agent_id),
            tiers = quote_list(self.tiers),
            statuses = quote_list(self.statuses),
            trusts = quote_list(self.trusts),
            now = self.now,
        )
    }

    /// The subset of the predicate the `vec0` metadata columns can express.
    ///
    /// `vec_memories` carries `tier/status/trust/kind/pinned` only, so agent,
    /// expiry and supersede are enforced after the scan. Pushing this subset
    /// into the KNN `MATCH` is what stops disqualified vectors from occupying
    /// top-k slots at all.
    pub fn vec_metadata_predicate(&self) -> String {
        fn codes(items: &[&str], f: fn(&str) -> i64) -> String {
            items
                .iter()
                .map(|s| f(s).to_string())
                .collect::<Vec<_>>()
                .join(",")
        }
        format!(
            "tier IN ({tiers}) AND status IN ({statuses}) AND trust IN ({trusts})",
            tiers = quote_list(self.tiers),
            statuses = codes(self.statuses, status_code),
            trusts = codes(self.trusts, trust_code),
        )
    }

    /// Rust mirror of [`Self::predicate`], evaluated on a resolved row.
    ///
    /// Used as a second, independent gate on rows SQL already admitted (stale
    /// vec0 metadata can disagree with the canonical row), so any disagreement
    /// fails closed instead of leaking.
    pub fn admits(&self, row: &CanonicalRow) -> bool {
        row.agent_id == self.agent_id
            && self.tiers.contains(&row.tier.as_str())
            && self.statuses.contains(&row.status.as_str())
            && self.trusts.contains(&row.trust.as_str())
            && row.expires_at.is_none_or(|e| e > self.now)
            && row.superseded_by_id.is_none()
            && row.successor_id.is_none()
            && row.last_referenced_at.is_none_or(|lr| lr >= row.created_at)
    }
}

/// Load the rows behind `extra_where` that satisfy the hard filter.
///
/// `extra_where` is appended to the canonical predicate, so the SQL gate always
/// applies; every returned row must then also pass [`HardFilter::admits`].
async fn load_admitted(
    store: &StoreHandle,
    filter: &HardFilter,
    extra_where: String,
) -> Result<Vec<CanonicalRow>> {
    let sql = format!(
        "SELECT {cols} FROM memories m WHERE {pred} AND {extra}",
        cols = CANONICAL_COLUMNS,
        pred = filter.predicate("m"),
        extra = extra_where,
    );
    let rows = store
        .read(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt.query_map([], row_from_sql)?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            Ok(out)
        })
        .await?;
    // Intersection with the Rust mirror: fail closed on any disagreement.
    Ok(rows.into_iter().filter(|r| filter.admits(r)).collect())
}

/// Resolve vec-leg rowids to admitted canonical rows, keyed by rowid.
///
/// A rowid missing from the map is either an orphaned vector (no canonical row)
/// or a row the hard filter rejected (foreign agent, expired, superseded, stale
/// metadata). Either way it can never surface.
pub(super) async fn load_by_rowids(
    store: &StoreHandle,
    filter: &HardFilter,
    rowids: &[i64],
) -> Result<HashMap<i64, CanonicalRow>> {
    if rowids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = rowids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let rows = load_admitted(store, filter, format!("m.id IN ({list})")).await?;
    Ok(rows.into_iter().map(|r| (r.rowid, r)).collect())
}

/// Re-apply the hard filter to a fused id set (the post-fuse structural gate).
///
/// RRF is rank-based, so a candidate that should never have been scored must be
/// dropped *after* the merge as well as before it.
pub(super) async fn surviving_public_ids(
    store: &StoreHandle,
    filter: &HardFilter,
    public_ids: &[String],
) -> Result<HashSet<String>> {
    if public_ids.is_empty() {
        return Ok(HashSet::new());
    }
    let list = public_ids
        .iter()
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(",");
    let rows = load_admitted(store, filter, format!("m.public_id IN ({list})")).await?;
    Ok(rows.into_iter().map(|r| r.public_id).collect())
}
