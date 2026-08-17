//! OpenClaw Markdown ingest.
//!
//! `MEMORY.md` and `memory/YYYY-MM-DD.md` are parsed into source items. Item
//! text is redacted and persisted through the normal memory write path; daily
//! files are also recorded as a session/turn log. A manifest in `meta` maps
//! `file:line` to a content hash and public id, making unchanged trees no-ops
//! while stale source references remain visible to operators.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::config::{Config, EmbedProvider};
use crate::embed::{HashEmbedder, OpenAiCompatEmbedder};
use crate::error::{Error, Result};
use crate::http::HttpConfig;
use crate::memory::{self, extract::Candidate, sessions};
use crate::storage::StoreHandle;
use crate::util::sha256_hex;

const MANIFEST_KEY: &str = "openclaw_ingest_manifest_v1";
const MAX_MARKDOWN_BYTES: u64 = 1_048_576;
const MAX_ITEMS_PER_FILE: usize = 10_000;
const MAX_ITEM_CHARS: usize = 4_000;

/// Counts returned by an ingest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestReport {
    pub seen: usize,
    pub new: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub stale: usize,
    pub skipped: usize,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedItem {
    text: String,
    source_ref: String,
    daily: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManifestEntry {
    hash: String,
    public_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    items: HashMap<String, ManifestEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    New,
    Update,
    Unchanged,
}

#[derive(Debug, Clone)]
struct PlannedItem {
    item: ParsedItem,
    hash: String,
    action: Action,
    old_public_id: Option<String>,
}

/// Run OpenClaw Markdown ingestion.
pub async fn run(cfg: &Config, path: &Path, dry_run: bool) -> Result<()> {
    let report = ingest(cfg, path, dry_run).await?;
    let prefix = if report.dry_run { "dry-run: " } else { "" };
    println!(
        "{prefix}ingest seen={} new={} updated={} unchanged={} stale={} skipped={}",
        report.seen, report.new, report.updated, report.unchanged, report.stale, report.skipped
    );
    Ok(())
}

/// Ingest a directory or one Markdown file and return its report.
pub async fn ingest(cfg: &Config, path: &Path, dry_run: bool) -> Result<IngestReport> {
    let files = discover_files(path)?;
    if files.is_empty() {
        return Err(Error::InvalidInput(format!(
            "no OpenClaw Markdown files found under {}",
            path.display()
        )));
    }
    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;
    let manifest = read_manifest(&store).await?;
    let mut planned = Vec::new();
    let mut seen_refs = HashMap::new();
    let mut skipped = 0usize;

    for (file, relative) in files {
        let (items, file_skipped) = parse_file(&file, &relative)?;
        skipped += file_skipped;
        for item in items {
            if seen_refs.insert(item.source_ref.clone(), ()).is_some() {
                return Err(Error::InvalidInput(format!(
                    "duplicate source reference {}",
                    item.source_ref
                )));
            }
            let hash = source_hash(&item.text, &item.source_ref);
            let action = match manifest.items.get(&item.source_ref) {
                Some(entry) if entry.hash == hash => Action::Unchanged,
                Some(_) => Action::Update,
                None => Action::New,
            };
            let old_public_id = manifest
                .items
                .get(&item.source_ref)
                .map(|entry| entry.public_id.clone());
            planned.push(PlannedItem {
                item,
                hash,
                action,
                old_public_id,
            });
        }
    }

    planned.sort_by(|a, b| a.item.source_ref.cmp(&b.item.source_ref));
    let stale = manifest
        .items
        .keys()
        .filter(|source_ref| !seen_refs.contains_key(*source_ref))
        .count();
    let mut new = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    let mut next_manifest = manifest;

    for planned_item in &planned {
        if planned_item.action == Action::Unchanged {
            unchanged += 1;
            continue;
        }
        match planned_item.action {
            Action::Unchanged => unreachable!(),
            Action::New => new += 1,
            Action::Update => updated += 1,
        }
        if dry_run {
            continue;
        }

        let public_id = if let Some(old_public_id) = &planned_item.old_public_id
            && store.get_memory(old_public_id.clone()).await?.is_some()
        {
            let row = store
                .get_memory(old_public_id.clone())
                .await?
                .expect("checked above");
            let redacted = crate::memory::redact::redact(&planned_item.item.text);
            let updated_row = store
                .update_memory_with_provenance(
                    row.id,
                    &redacted,
                    Some("file"),
                    Some("openclaw ingest: edited source item"),
                    Some("openclaw-ingest"),
                )
                .await?;
            store
                .set_source_ref(updated_row.id, &planned_item.item.source_ref)
                .await?;
            updated_row.public_id
        } else {
            let report = write_item(cfg, &store, &planned_item.item).await?;
            if !report.deduped {
                store
                    .set_source_ref(report.row.id, &planned_item.item.source_ref)
                    .await?;
            }
            report.row.public_id
        };

        if planned_item.item.daily {
            record_daily_turn(&store, &planned_item.item).await?;
        }
        next_manifest.items.insert(
            planned_item.item.source_ref.clone(),
            ManifestEntry {
                hash: planned_item.hash.clone(),
                public_id,
            },
        );
    }

    if !dry_run {
        write_manifest(&store, &next_manifest).await?;
    }
    Ok(IngestReport {
        seen: planned.len(),
        new,
        updated,
        unchanged,
        stale,
        skipped,
        dry_run,
    })
}

async fn read_manifest(store: &StoreHandle) -> Result<Manifest> {
    let key = MANIFEST_KEY.to_string();
    store
        .read(move |conn| {
            let value: Option<String> = conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| {
                    row.get(0)
                })
                .optional()?;
            match value {
                Some(value) => serde_json::from_str(&value).map_err(|error| {
                    crate::error::Error::Storage(format!(
                        "invalid OpenClaw ingest manifest: {error}"
                    ))
                }),
                None => Ok(Manifest::default()),
            }
        })
        .await
}

async fn write_manifest(store: &StoreHandle, manifest: &Manifest) -> Result<()> {
    let key = MANIFEST_KEY.to_string();
    let value = serde_json::to_string(manifest).map_err(|error| {
        crate::error::Error::Storage(format!("encode ingest manifest: {error}"))
    })?;
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![&key, &value],
            )?;
            Ok(())
        })
        .await
}

async fn write_item(
    cfg: &Config,
    store: &StoreHandle,
    item: &ParsedItem,
) -> Result<memory::PersistReport> {
    let candidate = Candidate {
        tier: "semantic".into(),
        kind: "fact".into(),
        text: item.text.clone(),
        importance: 0.5,
        confidence: 0.9,
        session_independent: true,
        source_seq: 0,
    };
    match cfg.embed.provider {
        EmbedProvider::None => {
            memory::persist::persist_candidate_degraded(
                store,
                &candidate,
                "openclaw-ingest-v1",
                "file",
                cfg.memory.pending_threshold,
            )
            .await
        }
        EmbedProvider::Hash => {
            let dim = store.embed_dim().await?;
            let embedder = HashEmbedder::new(dim);
            store.validate_embed_dim(&embedder).await?;
            memory::persist::persist_candidate_full(
                store,
                &candidate,
                &embedder,
                "openclaw-ingest-v1",
                "file",
                cfg.memory.pending_threshold,
                cfg.memory.dedup_threshold,
            )
            .await
        }
        EmbedProvider::OpenAiCompat => {
            if cfg.embed.model.is_empty() {
                return Err(Error::Config(
                    "embed.model is empty; set [embed] model or use provider = 'none'".into(),
                ));
            }
            let dim = store.embed_dim().await?;
            let http = HttpConfig::new(
                cfg.embed.base_url.clone(),
                cfg.embed.api_key.clone(),
                cfg.embed.timeout_secs,
            );
            let embedder = OpenAiCompatEmbedder::new(http, cfg.embed.model.clone(), dim);
            store.validate_embed_dim(&embedder).await?;
            memory::persist::persist_candidate_full(
                store,
                &candidate,
                &embedder,
                "openclaw-ingest-v1",
                "file",
                cfg.memory.pending_threshold,
                cfg.memory.dedup_threshold,
            )
            .await
        }
    }
}

async fn record_daily_turn(store: &StoreHandle, item: &ParsedItem) -> Result<()> {
    let session = match sessions::get_open_session(store, store.agent_id()).await? {
        Some(session) => session,
        None => sessions::open_session(store, store.agent_id()).await?,
    };
    let content = format!(
        "[untrusted-file {}] {}",
        item.source_ref,
        crate::memory::redact::redact(&item.text)
    );
    sessions::append_turn(store, session.id, "tool", &content).await?;
    Ok(())
}

fn discover_files(path: &Path) -> Result<Vec<(PathBuf, String)>> {
    if path.is_file() {
        let relative = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "MEMORY.md".into());
        return Ok(vec![(path.to_path_buf(), relative)]);
    }
    if !path.is_dir() {
        return Err(Error::InvalidInput(format!(
            "ingest path does not exist or is not a file/directory: {}",
            path.display()
        )));
    }

    let mut files = Vec::new();
    let memory = path.join("MEMORY.md");
    if memory.is_file() {
        files.push((memory, "MEMORY.md".into()));
    }
    let daily_dir = path.join("memory");
    if daily_dir.is_dir() {
        let mut daily = Vec::new();
        for entry in fs::read_dir(&daily_dir).map_err(|error| {
            Error::Storage(format!("cannot read {}: {error}", daily_dir.display()))
        })? {
            let entry = entry.map_err(|error| {
                Error::Storage(format!("cannot read {}: {error}", daily_dir.display()))
            })?;
            let child = entry.path();
            if child.is_file() && is_daily_markdown(&child) {
                let name = child
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                daily.push((child, format!("memory/{name}")));
            }
        }
        daily.sort_by(|a, b| a.1.cmp(&b.1));
        files.extend(daily);
    }
    Ok(files)
}

fn is_daily_markdown(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn parse_file(path: &Path, relative: &str) -> Result<(Vec<ParsedItem>, usize)> {
    let metadata = fs::metadata(path)
        .map_err(|error| Error::Storage(format!("cannot stat {}: {error}", path.display())))?;
    if metadata.len() > MAX_MARKDOWN_BYTES {
        return Err(Error::InvalidInput(format!(
            "Markdown file {} exceeds {} bytes",
            path.display(),
            MAX_MARKDOWN_BYTES
        )));
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| Error::Storage(format!("cannot read {}: {error}", path.display())))?;
    let mut items = Vec::new();
    let mut paragraph: Option<(usize, Vec<String>)> = None;
    let mut skipped = 0usize;

    fn flush(
        paragraph: &mut Option<(usize, Vec<String>)>,
        items: &mut Vec<ParsedItem>,
        relative: &str,
        skipped: &mut usize,
    ) {
        if let Some((line, lines)) = paragraph.take() {
            let text = normalize_text(&lines.join(" "));
            if text.is_empty() || text.chars().count() > MAX_ITEM_CHARS {
                *skipped += 1;
            } else {
                items.push(ParsedItem {
                    text,
                    source_ref: format!("{relative}:{line}"),
                    daily: relative.starts_with("memory/"),
                });
            }
        }
    }

    for (index, raw_line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            flush(&mut paragraph, &mut items, relative, &mut skipped);
            continue;
        }
        if let Some(item_text) = list_item(trimmed) {
            flush(&mut paragraph, &mut items, relative, &mut skipped);
            let text = normalize_text(item_text);
            if text.is_empty() || text.chars().count() > MAX_ITEM_CHARS {
                skipped += 1;
            } else {
                items.push(ParsedItem {
                    text,
                    source_ref: format!("{relative}:{line_number}"),
                    daily: relative.starts_with("memory/"),
                });
            }
            continue;
        }
        paragraph
            .get_or_insert_with(|| (line_number, Vec::new()))
            .1
            .push(trimmed.to_string());
    }
    flush(&mut paragraph, &mut items, relative, &mut skipped);
    if items.len() > MAX_ITEMS_PER_FILE {
        return Err(Error::InvalidInput(format!(
            "Markdown file {} contains more than {} items",
            path.display(),
            MAX_ITEMS_PER_FILE
        )));
    }
    Ok((items, skipped))
}

fn list_item(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    let bytes = line.as_bytes();
    let dot = bytes.iter().position(|byte| *byte == b'.')?;
    if dot == 0 || !bytes[..dot].iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    line.get(dot + 1..).map(str::trim_start)
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn source_hash(text: &str, source_ref: &str) -> String {
    sha256_hex(&format!("{}\n{}", text.trim(), source_ref))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_granularity_keeps_original_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MEMORY.md");
        fs::write(
            &file,
            "# heading\n\n- alpha beta\n\nparagraph one\ncontinued\n",
        )
        .unwrap();
        let (items, skipped) = parse_file(&file, "MEMORY.md").unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "alpha beta");
        assert_eq!(items[0].source_ref, "MEMORY.md:3");
        assert_eq!(items[1].text, "paragraph one continued");
        assert_eq!(items[1].source_ref, "MEMORY.md:5");
    }

    #[test]
    fn only_daily_markdown_files_are_selected() {
        assert!(is_daily_markdown(Path::new("memory/2026-09-24.md")));
        assert!(!is_daily_markdown(Path::new("memory/2026-9-24.md")));
        assert!(!is_daily_markdown(Path::new("memory/notes.md")));
    }
}
