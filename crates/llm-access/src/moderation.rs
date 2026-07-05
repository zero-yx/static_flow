//! In-memory keyword moderation gate enforced before upstream dispatch.
//!
//! This module is the request-side half of keyword moderation. The matching
//! algorithm itself lives in [`llm_access_core::moderation`]; here we own the
//! *caching contract* and the *per-request decision flow* that wrap it, for
//! both the Kiro and Codex provider surfaces.
//!
//! # Where the data lives (caching contract)
//!
//! The hot path reads only process memory and never queries Postgres. Postgres
//! is the durable control plane: read on startup / periodic refresh / admin
//! change, and written exactly once per *newly* banned hit.
//!
//! ```text
//!   ┌──────────────────── in process memory (RwLock<ModerationGateState>) ────────────────┐
//!   │  matcher : Aho-Corasick automaton compiled from every keyword                        │
//!   │  banned  : HashMap<session_key, hit_key>                                             │
//!   │  cleared : HashSet<session_key + matched_keyword>                                    │
//!   └─────────────────────────────────────────────────────────────────────────────────────┘
//!        ▲ reload(): full snapshot  — startup, every N minutes, and after any admin change
//!        │ ban_session(): ONE spawned INSERT per new ban (body + headers), off the hot path
//!        ▼
//!   ┌──────────────────────────────── Postgres (control plane) ───────────────────────────┐
//!   │  llm_moderation_keywords              llm_moderation_banned_sessions                  │
//!   └─────────────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Per-request decision flow ([`enforce_moderation`])
//!
//! Every provider hook funnels through one function so the three call sites
//! (Kiro-native, direct-Anthropic, Codex) stay identical. Work is done lazily:
//! a dormant gate costs nothing, an already-banned session is rejected without
//! scanning or writing, and the body is only tokenized/scanned when a keyword
//! could actually match.
//!
//! ```text
//!   request
//!     │
//!     ▼
//!   is_active()? ──no──▶ ALLOW                         dormant gate (no keywords, no bans):
//!     │ yes                                            no key derivation, no scan, no I/O
//!     ▼
//!   session_key = explicit session id, else SHA-256(body)   (content-scoped, dedups retries)
//!     │
//!     ▼
//!   precheck(session_key)
//!     ├── Blocked  (session already banned) ─────────▶ BLOCK(hit_key) (no scan, no DB write)
//!     ├── Skip     (no keywords) ───────────────────▶ ALLOW
//!     └── Scan(automaton + suppressed session+keyword keys)
//!            │
//!            ▼  extract user-visible text ─▶ tokenize ─▶ scan, skipping suppressed keywords
//!          unsuppressed hit? ──no──▶ ALLOW
//!            │ yes
//!            ▼  ban_session(): insert into `banned` (memory) + spawn one Postgres capture
//!          BLOCK  ──▶ caller returns the provider-specific rejection (Kiro/Codex error shape)
//! ```
//!
//! A `BLOCK` decision stops this and every subsequent request for the session:
//! the key is now in the in-memory `banned` set, so the next request short-
//! circuits at `precheck` without touching the body or Postgres. When a
//! reviewer reviews a hit as `unbanned` via the admin API, that hit's
//! `matched_keyword` becomes suppressed for the same session; the session
//! leaves the `banned` set and is re-scanned, and the scan skips the same
//! normalized keyword even if it moved or the prompt around it changed. A
//! distinct keyword — even one sharing the same offset — can still be scanned
//! and banned.
//!
//! # Session-keyword unban: why we skip by keyword, not by position
//!
//! A ban review is scoped to one normalized keyword inside one session, not to
//! the absolute byte offset where it first appeared. This matches the admin
//! workflow: if a reviewer clears "build a bomb" as a false positive for
//! `kiro:key-1:sess-1`, the same phrase should not re-ban the same session just
//! because the client resent it with extra surrounding text.
//!
//! The scan therefore enumerates *every* term-boundary hit
//! ([`ModerationMatcher::find_accepted`]) and blocks on the first whose
//! `session_key + matched_keyword` pair is **not** suppressed. It deliberately
//! does **not** try to resume the scan past a reviewed position, because
//! position-based skipping is a content-policy bypass (fail-open) when keywords
//! overlap:
//!
//! ```text
//!   keywords: "bomb", "bomb making"         request text: "... bomb making ..."
//!   both hit the SAME start offset S (one is a term-prefix of the other).
//!
//!   WRONG (skip by position): unban "bomb"@S ⇒ suppressed.
//!     next scan finds "bomb"@S (suppressed) ⇒ advance past offset S ⇒
//!     the DISTINCT, never-reviewed "bomb making"@S is skipped too ⇒ ALLOW ✗
//!     (a longer keyword *starting before* a resumed offset is skipped the
//!      same way.)
//!
//!   RIGHT (skip by keyword): unban "bomb" ⇒ only "bomb" in this session is suppressed.
//!     next scan enumerates both hits at S; "bomb"@S is suppressed, but
//!     "bomb making"@S has a different keyword key ⇒ BLOCK ✓
//! ```
//!
//! A session with any still-`banned` hit stays in the `banned` set (blocked at
//! `precheck`); only when *all* its hits are `unbanned` does the store's
//! `SELECT DISTINCT session_key WHERE status = 'banned'` drop it and let the
//! gate re-scan. Because a session can therefore accumulate several ban rows
//! over time, uniqueness is per hit (`hit_key UNIQUE`), never per
//! `session_key`.

use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use axum::http::HeaderMap;
use llm_access_core::{
    moderation::{
        extract_moderation_text, normalize_moderation_text, ModerationMatch, ModerationMatcher,
    },
    store::{
        AdminModerationStore, AuthenticatedKey, EmptyAdminModerationStore, ModerationKeyword,
        NewModerationBannedSession,
    },
};
use llm_access_kiro::anthropic::types::MessagesRequest;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Provider label recorded for Kiro-surface bans (native and direct Anthropic
/// upstream dispatch).
pub(crate) const MODERATION_PROVIDER_KIRO: &str = "kiro";
/// Provider label recorded for Codex-surface bans.
pub(crate) const MODERATION_PROVIDER_CODEX: &str = "codex";

/// Client-facing rejection message for moderated sessions.
pub(crate) const MODERATION_BLOCKED_MESSAGE: &str = "This session has been blocked by the gateway \
                                                     content policy. Contact the administrator if \
                                                     you believe this is a mistake.";

const MODERATION_REFRESH_INTERVAL_ENV: &str = "LLM_ACCESS_MODERATION_REFRESH_SECONDS";
const DEFAULT_MODERATION_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Header names whose values are redacted before a ban record is persisted.
const REDACTED_HEADER_NAMES: [&str; 5] =
    ["authorization", "proxy-authorization", "x-api-key", "x-admin-token", "cookie"];

/// Hot-path decision for one request before any keyword scan runs.
pub(crate) enum ModerationPrecheck {
    /// Session key is already banned: reject without scanning or writing.
    Blocked { review_id: String },
    /// The gate has no keywords / no loaded snapshot yet: skip scanning.
    Skip,
    /// Scan the request text with the compiled automaton.
    Scan(ModerationScanPlan),
}

pub(crate) struct ModerationScanPlan {
    matcher: Arc<ModerationMatcher>,
    keyword_set_hash: String,
    suppressed_keyword_keys: Arc<HashSet<String>>,
}

#[derive(Default)]
struct ModerationGateState {
    matcher: Option<Arc<ModerationMatcher>>,
    keyword_count: usize,
    banned: HashMap<String, String>,
    suppressed_keyword_keys: Arc<HashSet<String>>,
    keyword_set_hash: String,
    loaded: bool,
    loaded_at_ms: Option<i64>,
}

/// Runtime counters and cache sizes surfaced on the admin overview.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModerationGateStats {
    pub loaded: bool,
    pub loaded_at_ms: Option<i64>,
    pub keyword_count: usize,
    pub banned_session_count: usize,
    pub suppressed_hit_count: usize,
    pub blocked_requests_total: u64,
    pub persist_failures_total: u64,
}

/// Shared keyword moderation gate. See module docs for the caching contract.
pub struct ModerationGate {
    store: Arc<dyn AdminModerationStore>,
    state: RwLock<ModerationGateState>,
    blocked_requests_total: AtomicU64,
    persist_failures_total: Arc<AtomicU64>,
}

impl ModerationGate {
    /// Create a gate backed by a persistent moderation store. The gate starts
    /// unloaded (fail-open) until the first [`Self::reload`] succeeds.
    pub fn new(store: Arc<dyn AdminModerationStore>) -> Arc<Self> {
        Arc::new(Self {
            store,
            state: RwLock::new(ModerationGateState::default()),
            blocked_requests_total: AtomicU64::new(0),
            persist_failures_total: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Create a permanently empty gate for tests and store-less runtimes.
    pub fn disabled() -> Arc<Self> {
        let gate = Self::new(Arc::new(EmptyAdminModerationStore));
        {
            let mut state = gate.state.write().expect("moderation gate lock poisoned");
            state.loaded = true;
        }
        gate
    }

    /// Reload the keyword automaton and session sets from the store.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let snapshot = self.store.load_moderation_runtime_snapshot().await?;
        let keyword_set_hash = moderation_keyword_set_hash(&snapshot.keywords);
        // Rebuild the matcher (Aho-Corasick automaton + a seeded Jieba
        // dictionary clone) only when the keyword set actually changed. The
        // banned/suppressed sets below still refresh every reload; reusing the
        // matcher avoids cloning the ~350k-entry dictionary on no-op refreshes.
        let reusable_matcher = {
            let state = self.state.read().expect("moderation gate lock poisoned");
            if state.keyword_set_hash == keyword_set_hash {
                state.matcher.clone()
            } else {
                None
            }
        };
        let matcher = match reusable_matcher {
            Some(existing) => Some(existing),
            None => ModerationMatcher::build(
                snapshot
                    .keywords
                    .iter()
                    .map(|keyword| (keyword.keyword.as_str(), keyword.categories.clone())),
            )?
            .map(Arc::new),
        };
        let keyword_count = matcher
            .as_ref()
            .map(|matcher| matcher.pattern_count())
            .unwrap_or(0);
        let suppressed_keyword_keys: HashSet<String> = snapshot
            .suppressed_hits
            .into_iter()
            .map(|hit| moderation_suppressed_keyword_key(&hit.session_key, &hit.matched_keyword))
            .collect();
        let mut state = self.state.write().expect("moderation gate lock poisoned");
        state.matcher = matcher;
        state.keyword_count = keyword_count;
        state.banned = snapshot
            .banned_sessions
            .into_iter()
            .map(|ban| (ban.session_key, ban.hit_key))
            .collect();
        state.suppressed_keyword_keys = Arc::new(suppressed_keyword_keys);
        state.keyword_set_hash = keyword_set_hash;
        state.loaded = true;
        state.loaded_at_ms = Some(now_ms());
        Ok(())
    }

    /// Reload immediately, then keep the snapshot fresh on a fixed interval as
    /// a safety net for out-of-band store changes.
    pub async fn run_refresh_loop(self: Arc<Self>) {
        let interval = moderation_refresh_interval();
        loop {
            match self.reload().await {
                Ok(()) => {
                    let stats = self.stats();
                    tracing::debug!(
                        keyword_count = stats.keyword_count,
                        banned_session_count = stats.banned_session_count,
                        suppressed_hit_count = stats.suppressed_hit_count,
                        "moderation gate snapshot refreshed"
                    );
                },
                Err(err) => {
                    tracing::warn!("moderation gate snapshot refresh failed: {err:#}");
                },
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Whether the gate can affect any request. Dormant means loaded with no
    /// keywords and no banned sessions, or not loaded yet — in which case the
    /// hot path can skip all moderation work (session-key derivation, text
    /// extraction, and scanning) entirely.
    pub(crate) fn is_active(&self) -> bool {
        let state = self.state.read().expect("moderation gate lock poisoned");
        state.loaded && (state.matcher.is_some() || !state.banned.is_empty())
    }

    /// Classify one request before scanning.
    pub(crate) fn precheck(&self, session_key: Option<&str>) -> ModerationPrecheck {
        let state = self.state.read().expect("moderation gate lock poisoned");
        if !state.loaded {
            return ModerationPrecheck::Skip;
        }
        if let Some(session_key) = session_key {
            if let Some(review_id) = state.banned.get(session_key) {
                return ModerationPrecheck::Blocked {
                    review_id: review_id.clone(),
                };
            }
        }
        match &state.matcher {
            Some(matcher) => ModerationPrecheck::Scan(ModerationScanPlan {
                matcher: Arc::clone(matcher),
                keyword_set_hash: state.keyword_set_hash.clone(),
                suppressed_keyword_keys: Arc::clone(&state.suppressed_keyword_keys),
            }),
            None => ModerationPrecheck::Skip,
        }
    }

    /// Count one rejected request (new ban or repeat block).
    pub(crate) fn note_blocked_request(&self) {
        self.blocked_requests_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Ban a session in memory and persist the capture exactly once. Repeat
    /// calls for an already-banned session key never reach the store.
    pub(crate) fn ban_session(&self, record: NewModerationBannedSession) {
        let newly_banned = {
            let mut state = self.state.write().expect("moderation gate lock poisoned");
            state.state_ban(&record.session_key, &record.hit_key)
        };
        if !newly_banned {
            return;
        }
        tracing::warn!(
            session_key = %record.session_key,
            provider = %record.provider,
            key_id = %record.key_id,
            matched_keyword = %record.matched_keyword,
            endpoint = %record.endpoint,
            "moderation keyword hit banned session"
        );
        let store = Arc::clone(&self.store);
        let persist_failures = Arc::clone(&self.persist_failures_total);
        tokio::spawn(async move {
            let session_key = record.session_key.clone();
            match store.record_moderation_banned_session(record).await {
                Ok(true) => {},
                Ok(false) => tracing::debug!(
                    session_key = %session_key,
                    "moderation banned session was already recorded"
                ),
                Err(err) => {
                    persist_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        session_key = %session_key,
                        "failed to persist moderation banned session: {err:#}"
                    );
                },
            }
        });
    }

    /// Runtime stats for the admin overview.
    pub(crate) fn stats(&self) -> ModerationGateStats {
        let state = self.state.read().expect("moderation gate lock poisoned");
        ModerationGateStats {
            loaded: state.loaded,
            loaded_at_ms: state.loaded_at_ms,
            keyword_count: state.keyword_count,
            banned_session_count: state.banned.len(),
            suppressed_hit_count: state.suppressed_keyword_keys.len(),
            blocked_requests_total: self.blocked_requests_total.load(Ordering::Relaxed),
            persist_failures_total: self.persist_failures_total.load(Ordering::Relaxed),
        }
    }
}

impl ModerationGateState {
    fn state_ban(&mut self, session_key: &str, hit_key: &str) -> bool {
        self.banned
            .insert(session_key.to_string(), hit_key.to_string())
            .is_none()
    }
}

/// Outcome of the moderation gate for one request.
pub(crate) enum ModerationDecision {
    /// Allow the request to proceed to upstream dispatch.
    Allow,
    /// Reject the request: the session is banned, either already or by a
    /// keyword hit this call just recorded. The caller returns its
    /// provider-specific rejection response.
    Block { review_id: String },
}

pub(crate) fn moderation_blocked_message(review_id: &str) -> String {
    format!("{MODERATION_BLOCKED_MESSAGE} Moderation review id: {review_id}.")
}

/// Immutable request facts the gate needs to classify a request and, on a
/// keyword hit, build the ban capture.
pub(crate) struct ModerationRequest<'a> {
    /// Provider family label ([`MODERATION_PROVIDER_KIRO`] / `_CODEX`).
    pub provider: &'a str,
    pub key: &'a AuthenticatedKey,
    /// Explicit client session id when the request carried a stable one; the
    /// gate falls back to a content-derived key otherwise.
    pub session_id: Option<&'a str>,
    pub endpoint: &'a str,
    pub model: &'a str,
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
    pub client_ip: &'a str,
}

/// Enforce the keyword moderation gate for one request, shared by every
/// provider dispatch hook. `extract_text` yields the raw request text to scan;
/// the matcher normalizes it through its keyword-seeded analyzer. It is invoked
/// only when the gate must actually scan (keywords configured and the session is
/// not already banned), so the caller pays the extraction cost only when it can
/// change the outcome. Returns [`ModerationDecision::Block`] when the request
/// must be rejected.
pub(crate) fn enforce_moderation(
    gate: &ModerationGate,
    request: ModerationRequest<'_>,
    extract_text: impl FnOnce() -> String,
) -> ModerationDecision {
    // Dormant gate (no keywords, no bans): do zero work — no key derivation,
    // no text extraction, no scan.
    if !gate.is_active() {
        return ModerationDecision::Allow;
    }
    let session_key = match request.session_id {
        Some(session_id) => {
            moderation_session_key(request.provider, &request.key.key_id, session_id)
        },
        None => derived_moderation_session_key(request.provider, &request.key.key_id, request.body),
    };
    match gate.precheck(Some(&session_key)) {
        ModerationPrecheck::Blocked {
            review_id,
        } => {
            gate.note_blocked_request();
            ModerationDecision::Block {
                review_id,
            }
        },
        ModerationPrecheck::Scan(plan) => {
            // Normalize through the matcher's keyword-seeded analyzer so keyword
            // spans survive as tokens in the request's surrounding Chinese.
            let scanned = plan.matcher.normalize(&extract_text());
            // Enumerate every term-boundary hit and block on the first whose
            // session+keyword pair is NOT suppressed. This is intentionally
            // broader than hit_key: once a reviewer clears a matched keyword in
            // one session, the same normalized keyword may move in that
            // session without being re-banned. Distinct co-located keywords are
            // still evaluated independently, so clearing "bomb" never masks
            // "bomb making".
            let Some(hit) = plan
                .matcher
                .find_accepted(&scanned, |keyword, _start, _end| {
                    let suppressed_keyword_key =
                        moderation_suppressed_keyword_key(&session_key, keyword);
                    !plan
                        .suppressed_keyword_keys
                        .contains(&suppressed_keyword_key)
                })
            else {
                return ModerationDecision::Allow;
            };
            let hit_key = moderation_hit_key(&session_key, &scanned, &hit);
            let review_id = hit_key.clone();
            let match_prefix_sha256 = moderation_match_prefix_sha256(&scanned, hit.match_start);
            let match_start = hit.match_start.min(i64::MAX as usize) as i64;
            let match_end = hit.match_end.min(i64::MAX as usize) as i64;
            gate.note_blocked_request();
            gate.ban_session(NewModerationBannedSession {
                hit_key,
                session_key,
                provider: request.provider.to_string(),
                key_id: request.key.key_id.clone(),
                key_name: request.key.key_name.clone(),
                session_id: request.session_id.unwrap_or_default().to_string(),
                matched_keyword: hit.keyword,
                matched_categories: hit.categories,
                matched_context: hit.context,
                match_start,
                match_end,
                match_prefix_sha256,
                keyword_set_hash: plan.keyword_set_hash,
                endpoint: request.endpoint.to_string(),
                model: request.model.to_string(),
                client_ip: request.client_ip.to_string(),
                request_headers_json: redacted_headers_json(request.headers),
                request_body_json: moderation_body_text(request.body),
                banned_at_ms: now_ms(),
            });
            ModerationDecision::Block {
                review_id,
            }
        },
        ModerationPrecheck::Skip => ModerationDecision::Allow,
    }
}

/// Apply the per-key route switch before entering the global moderation gate.
///
/// Request dispatch has already selected a concrete key route at this point:
///
/// ```text
/// authenticated key -> selected provider route -> per-key moderation switch
///                                           \-> global keyword/session gate
/// ```
///
/// Keep this switch outside [`ModerationGate`]. The gate owns one global
/// in-memory keyword/session snapshot, while the enablement bit belongs to the
/// key route chosen for this request.
pub(crate) fn enforce_key_moderation(
    moderation_enabled: bool,
    gate: &ModerationGate,
    request: ModerationRequest<'_>,
    extract_text: impl FnOnce() -> String,
) -> ModerationDecision {
    if !moderation_enabled {
        return ModerationDecision::Allow;
    }
    enforce_moderation(gate, request, extract_text)
}

/// Compose the runtime ban key.
pub(crate) fn moderation_session_key(provider: &str, key_id: &str, session_id: &str) -> String {
    format!("{provider}:{key_id}:{session_id}")
}

fn moderation_suppressed_keyword_key(session_key: &str, matched_keyword: &str) -> String {
    format!("{session_key}:{matched_keyword}")
}

pub(crate) fn moderation_keyword_set_hash(keywords: &[ModerationKeyword]) -> String {
    let mut normalized: Vec<String> = keywords
        .iter()
        .map(|keyword| normalize_moderation_text(&keyword.keyword))
        .filter(|keyword| !keyword.is_empty())
        .collect();
    normalized.sort_unstable();
    let mut hasher = Sha256::new();
    for keyword in normalized {
        hasher.update(keyword.as_bytes());
        hasher.update([0]);
    }
    finish_sha256_hex(hasher)
}

pub(crate) fn moderation_match_prefix_sha256(normalized_text: &str, match_start: usize) -> String {
    let end = match_start.min(normalized_text.len());
    let mut hasher = Sha256::new();
    hasher.update(&normalized_text.as_bytes()[..end]);
    finish_sha256_hex(hasher)
}

pub(crate) fn moderation_hit_key(
    session_key: &str,
    normalized_text: &str,
    hit: &ModerationMatch,
) -> String {
    moderation_hit_key_parts(
        session_key,
        normalized_text,
        &hit.keyword,
        hit.match_start,
        hit.match_end,
    )
}

/// Stable identity of one keyword hit: which session, which keyword, where, and
/// the hash of the preceding content — so the same reviewed hit stays
/// suppressed only while its preceding context is byte-identical, and distinct
/// keywords at the same offset get distinct keys.
pub(crate) fn moderation_hit_key_parts(
    session_key: &str,
    normalized_text: &str,
    keyword: &str,
    match_start: usize,
    match_end: usize,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_key.as_bytes());
    hasher.update([0]);
    hasher.update(keyword.as_bytes());
    hasher.update([0]);
    hasher.update(match_start.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(match_end.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(moderation_match_prefix_sha256(normalized_text, match_start).as_bytes());
    finish_sha256_hex(hasher)
}

/// Derive a stable content-scoped ban key for requests without any session id
/// so identical retries are deduplicated in memory and by the store conflict.
pub(crate) fn derived_moderation_session_key(provider: &str, key_id: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body);
    let digest = hasher.finalize();
    let mut preview = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        preview.push_str(&format!("{byte:02x}"));
    }
    format!("{provider}:{key_id}:content:{preview}")
}

fn finish_sha256_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

/// Serialize request headers to a JSON object with sensitive values redacted.
pub(crate) fn redacted_headers_json(headers: &HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for name in headers.keys() {
        let key = name.as_str().to_ascii_lowercase();
        let values: Vec<serde_json::Value> = headers
            .get_all(name)
            .iter()
            .map(|value| {
                if REDACTED_HEADER_NAMES.contains(&key.as_str()) {
                    serde_json::Value::String("<redacted>".to_string())
                } else {
                    serde_json::Value::String(value.to_str().unwrap_or("<non-utf8>").to_string())
                }
            })
            .collect();
        match values.len() {
            0 => {},
            1 => {
                map.insert(key, values.into_iter().next().expect("one header value"));
            },
            _ => {
                map.insert(key, serde_json::Value::Array(values));
            },
        }
    }
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

/// Capture the request body verbatim for review. The column is `TEXT`, so the
/// exact wire bytes are preserved (invalid UTF-8 is lossily replaced) rather
/// than reparsed and re-serialized through JSONB.
pub(crate) fn moderation_body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Collect the user-visible request text for a parsed Anthropic messages
/// request. Returns raw (un-normalized) text; the moderation matcher normalizes
/// it through its keyword-seeded analyzer inside [`enforce_moderation`].
pub(crate) fn moderation_text_for_kiro(payload: &MessagesRequest) -> String {
    let mut collected = String::new();
    if let Some(system) = &payload.system {
        for block in system {
            if !block.text.is_empty() {
                if !collected.is_empty() {
                    collected.push('\n');
                }
                collected.push_str(&block.text);
            }
        }
    }
    for message in &payload.messages {
        match &message.content {
            serde_json::Value::String(text) => {
                if !text.is_empty() {
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(text);
                }
            },
            other => {
                let extracted = extract_moderation_text(other);
                if !extracted.is_empty() {
                    if !collected.is_empty() {
                        collected.push('\n');
                    }
                    collected.push_str(&extracted);
                }
            },
        }
    }
    collected
}

/// Collect the user-visible request text from a raw JSON request body (Codex
/// surface). Returns raw (un-normalized) text; the matcher normalizes it inside
/// [`enforce_moderation`]. `None` when the body is not JSON.
pub(crate) fn moderation_text_for_json_body(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    Some(extract_moderation_text(&value))
}

fn moderation_refresh_interval() -> Duration {
    std::env::var(MODERATION_REFRESH_INTERVAL_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.clamp(15, 24 * 60 * 60)))
        .unwrap_or(DEFAULT_MODERATION_REFRESH_INTERVAL)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use axum::http::{HeaderMap, HeaderValue};
    use llm_access_core::store::{
        page_moderation_keywords, AdminModerationBannedSessionPageQuery,
        AdminModerationKeywordPageQuery, AdminPageRequest, ModerationBannedSession,
        ModerationBannedSessionDetail, ModerationBannedSessionRef, ModerationBannedSessionsPage,
        ModerationCategory, ModerationKeyword, ModerationKeywordImportOutcome,
        ModerationKeywordsPage, ModerationRuntimeSnapshot, ModerationSuppressedHit,
        NewModerationCategory, NewModerationKeyword,
    };
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct MemoryModerationStore {
        snapshot: Mutex<ModerationRuntimeSnapshot>,
        records: Mutex<Vec<NewModerationBannedSession>>,
    }

    impl MemoryModerationStore {
        fn with_snapshot(snapshot: ModerationRuntimeSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(snapshot),
                records: Mutex::new(Vec::new()),
            }
        }

        fn records(&self) -> Vec<NewModerationBannedSession> {
            self.records.lock().expect("records lock").clone()
        }
    }

    #[async_trait]
    impl AdminModerationStore for MemoryModerationStore {
        async fn load_moderation_runtime_snapshot(
            &self,
        ) -> anyhow::Result<ModerationRuntimeSnapshot> {
            Ok(self.snapshot.lock().expect("snapshot lock").clone())
        }

        async fn list_moderation_categories(&self) -> anyhow::Result<Vec<ModerationCategory>> {
            Ok(Vec::new())
        }

        async fn add_moderation_categories(
            &self,
            _categories: Vec<NewModerationCategory>,
        ) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn delete_moderation_category(
            &self,
            _code: &str,
        ) -> anyhow::Result<Option<ModerationCategory>> {
            Ok(None)
        }

        async fn list_moderation_keywords(&self) -> anyhow::Result<Vec<ModerationKeyword>> {
            Ok(self
                .snapshot
                .lock()
                .expect("snapshot lock")
                .keywords
                .clone())
        }

        async fn list_moderation_keywords_page(
            &self,
            page: AdminPageRequest,
            query: &AdminModerationKeywordPageQuery,
        ) -> anyhow::Result<ModerationKeywordsPage> {
            Ok(page_moderation_keywords(
                self.snapshot
                    .lock()
                    .expect("snapshot lock")
                    .keywords
                    .clone(),
                query,
                page,
            ))
        }

        async fn add_moderation_keywords(
            &self,
            _keywords: Vec<NewModerationKeyword>,
        ) -> anyhow::Result<ModerationKeywordImportOutcome> {
            Ok(ModerationKeywordImportOutcome::default())
        }

        async fn delete_moderation_keyword(
            &self,
            _id: i64,
        ) -> anyhow::Result<Option<ModerationKeyword>> {
            Ok(None)
        }

        async fn record_moderation_banned_session(
            &self,
            record: NewModerationBannedSession,
        ) -> anyhow::Result<bool> {
            let mut records = self.records.lock().expect("records lock");
            if records
                .iter()
                .any(|existing| existing.hit_key == record.hit_key)
            {
                return Ok(false);
            }
            records.push(record);
            Ok(true)
        }

        async fn list_moderation_banned_sessions(
            &self,
            page: AdminPageRequest,
            _query: &AdminModerationBannedSessionPageQuery,
        ) -> anyhow::Result<ModerationBannedSessionsPage> {
            Ok(ModerationBannedSessionsPage {
                sessions: Vec::new(),
                total: 0,
                limit: page.limit,
                offset: page.offset,
                has_more: false,
            })
        }

        async fn get_moderation_banned_session(
            &self,
            _id: i64,
        ) -> anyhow::Result<Option<ModerationBannedSessionDetail>> {
            Ok(None)
        }

        async fn set_moderation_banned_session_status(
            &self,
            _id: i64,
            _status: &str,
            _review_note: Option<&str>,
            _reviewed_at_ms: i64,
        ) -> anyhow::Result<Option<ModerationBannedSession>> {
            Ok(None)
        }
    }

    fn moderation_keyword(id: i64, keyword: &str) -> ModerationKeyword {
        ModerationKeyword {
            id,
            keyword: keyword.to_string(),
            categories: Vec::new(),
            note: None,
            source: "txt".to_string(),
            created_at_ms: 1,
        }
    }

    fn sample_key() -> AuthenticatedKey {
        AuthenticatedKey {
            key_id: "key-1".to_string(),
            key_name: "external".to_string(),
            provider_type: "kiro".to_string(),
            protocol_family: "anthropic".to_string(),
            status: "active".to_string(),
            quota_billable_limit: 0,
            billable_tokens_used: 0,
        }
    }

    #[test]
    fn session_key_uses_provider_key_and_session() {
        assert_eq!(
            moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-9"),
            "kiro:key-1:sess-9"
        );
    }

    #[test]
    fn keyword_set_hash_uses_analyzer_terms() {
        let raw = vec![moderation_keyword(1, "口交")];
        let legacy_canonical = vec![moderation_keyword(1, "口 交")];
        let different = vec![moderation_keyword(1, "接口交互")];

        assert_eq!(
            moderation_keyword_set_hash(&raw),
            moderation_keyword_set_hash(&legacy_canonical)
        );
        assert_ne!(moderation_keyword_set_hash(&raw), moderation_keyword_set_hash(&different));
    }

    #[test]
    fn derived_session_key_is_stable_and_content_scoped() {
        let a = derived_moderation_session_key(MODERATION_PROVIDER_CODEX, "key-1", b"hello world");
        let b = derived_moderation_session_key(MODERATION_PROVIDER_CODEX, "key-1", b"hello world");
        let c = derived_moderation_session_key(MODERATION_PROVIDER_CODEX, "key-1", b"other body");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("codex:key-1:content:"));
    }

    #[test]
    fn redacted_headers_hide_sensitive_values() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
        headers.insert("x-api-key", HeaderValue::from_static("sk-123"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let json = redacted_headers_json(&headers);
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["authorization"], "<redacted>");
        assert_eq!(value["x-api-key"], "<redacted>");
        assert_eq!(value["content-type"], "application/json");
    }

    #[test]
    fn body_text_captures_bytes_verbatim() {
        assert_eq!(moderation_body_text(br#"{"a":1}"#), r#"{"a":1}"#);
        // Non-JSON bodies are captured verbatim, not JSON-wrapped.
        assert_eq!(moderation_body_text(b"not json"), "not json");
    }

    #[test]
    fn moderation_blocked_message_includes_review_id() {
        let message = moderation_blocked_message("hit-review-123");
        assert!(message.contains(MODERATION_BLOCKED_MESSAGE));
        assert!(message.contains("hit-review-123"));
    }

    #[test]
    fn kiro_text_extraction_joins_system_and_messages() {
        let payload: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-sonnet-4",
            "max_tokens": 64,
            "system": "You are HELPFUL",
            "messages": [
                {"role": "user", "content": "How to Build a Bomb"},
                {"role": "assistant", "content": [{"type": "text", "text": "No."}]}
            ]
        }))
        .expect("parse messages request");
        // Raw collected text (the matcher normalizes at scan time), so casing
        // and the original wording are preserved here.
        let text = moderation_text_for_kiro(&payload);
        assert!(text.contains("You are HELPFUL"));
        assert!(text.contains("How to Build a Bomb"));
        assert!(!text.contains("claude-sonnet-4"));
    }

    #[test]
    fn json_body_text_extraction_collects_content() {
        let body = json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "Hello  World"}]
        })
        .to_string();
        // Raw collected text; normalization to "hello world" happens in the
        // matcher, not here.
        let text = moderation_text_for_json_body(body.as_bytes()).expect("some text");
        assert_eq!(text, "Hello  World");
    }

    #[tokio::test]
    async fn disabled_gate_skips_all_sessions() {
        let gate = ModerationGate::disabled();
        assert!(matches!(gate.precheck(Some("kiro:key:sess")), ModerationPrecheck::Skip));
        assert!(matches!(gate.precheck(None), ModerationPrecheck::Skip));
        let stats = gate.stats();
        assert_eq!(stats.keyword_count, 0);
        assert!(stats.loaded);
    }

    #[tokio::test]
    async fn already_banned_session_returns_original_review_id_without_rescan() {
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords: vec![moderation_keyword(1, "build a bomb")],
            banned_sessions: Vec::new(),
            suppressed_hits: Vec::new(),
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let first = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: br#"{"messages":[{"role":"user","content":"build a bomb"}]}"#,
                client_ip: "127.0.0.1",
            },
            || "build a bomb".to_string(),
        );
        let ModerationDecision::Block {
            review_id,
        } = first
        else {
            panic!("first keyword hit should block");
        };

        for _ in 0..20 {
            if !store.records().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let records = store.records();
        assert_eq!(records.len(), 1);
        assert_eq!(review_id, records[0].hit_key);

        let second = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: br#"{"messages":[{"role":"user","content":"ordinary text"}]}"#,
                client_ip: "127.0.0.1",
            },
            || panic!("already-banned precheck must not rescan request text"),
        );
        assert!(matches!(
            second,
            ModerationDecision::Block { review_id: ref id } if id == &review_id
        ));
    }

    #[tokio::test]
    async fn disabled_key_moderation_skips_existing_ban_without_rescan() {
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords: vec![moderation_keyword(1, "build a bomb")],
            banned_sessions: vec![ModerationBannedSessionRef {
                session_key: moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-1"),
                hit_key: "review-existing".to_string(),
            }],
            suppressed_hits: Vec::new(),
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_key_moderation(
            false,
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: br#"{"messages":[{"role":"user","content":"ordinary text"}]}"#,
                client_ip: "127.0.0.1",
            },
            || panic!("disabled per-key moderation must not scan text"),
        );

        assert!(matches!(decision, ModerationDecision::Allow));
        assert!(store.records().is_empty());
        assert_eq!(gate.stats().blocked_requests_total, 0);
    }

    #[tokio::test]
    async fn disabled_key_moderation_skips_keyword_hit_without_recording_ban() {
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords: vec![moderation_keyword(1, "build a bomb")],
            banned_sessions: Vec::new(),
            suppressed_hits: Vec::new(),
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_key_moderation(
            false,
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: br#"{"messages":[{"role":"user","content":"build a bomb"}]}"#,
                client_ip: "127.0.0.1",
            },
            || panic!("disabled per-key moderation must not extract text"),
        );

        assert!(matches!(decision, ModerationDecision::Allow));
        assert!(store.records().is_empty());
        assert_eq!(gate.stats().blocked_requests_total, 0);
    }

    #[tokio::test]
    async fn unbanned_hit_is_suppressed_and_later_hit_in_same_session_blocks() {
        let keywords =
            vec![moderation_keyword(1, "build a bomb"), moderation_keyword(2, "carding tutorial")];
        let keyword_set_hash = moderation_keyword_set_hash(&keywords);
        let text = normalize_moderation_text(
            "reviewed false positive: build a bomb. New content asks for carding tutorial.",
        );
        let matcher = ModerationMatcher::build(
            keywords
                .iter()
                .map(|keyword| (keyword.keyword.as_str(), keyword.categories.clone())),
        )
        .expect("build matcher")
        .expect("matcher");
        let reviewed_hit = matcher.find(&text).expect("reviewed hit");
        assert_eq!(reviewed_hit.keyword, "build a bomb");

        let session_key = moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-1");
        let reviewed_hit_key = moderation_hit_key(&session_key, &text, &reviewed_hit);
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords,
            banned_sessions: Vec::new(),
            suppressed_hits: vec![ModerationSuppressedHit {
                session_key: session_key.clone(),
                hit_key: reviewed_hit_key.clone(),
                matched_keyword: reviewed_hit.keyword.clone(),
                match_start: reviewed_hit.match_start as i64,
                match_end: reviewed_hit.match_end as i64,
                match_prefix_sha256: moderation_match_prefix_sha256(
                    &text,
                    reviewed_hit.match_start,
                ),
                keyword_set_hash,
            }],
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: b"{}",
                client_ip: "127.0.0.1",
            },
            || text.clone(),
        );
        assert!(matches!(decision, ModerationDecision::Block { .. }));

        for _ in 0..20 {
            if !store.records().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let records = store.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_key, session_key);
        assert_eq!(records[0].matched_keyword, "carding tutorial");
        assert_ne!(records[0].hit_key, reviewed_hit_key);
        assert!(records[0].match_start > reviewed_hit.match_start as i64);
    }

    #[tokio::test]
    async fn changed_keyword_at_reviewed_position_is_not_suppressed() {
        let keywords =
            vec![moderation_keyword(1, "build a bomb"), moderation_keyword(2, "carding tutorial")];
        let keyword_set_hash = moderation_keyword_set_hash(&keywords);
        let reviewed_text = normalize_moderation_text("reviewed false positive: build a bomb");
        let changed_text = normalize_moderation_text("reviewed false positive: carding tutorial");
        let matcher = ModerationMatcher::build(
            keywords
                .iter()
                .map(|keyword| (keyword.keyword.as_str(), keyword.categories.clone())),
        )
        .expect("build matcher")
        .expect("matcher");
        let reviewed_hit = matcher.find(&reviewed_text).expect("reviewed hit");
        assert_eq!(reviewed_hit.keyword, "build a bomb");

        let session_key = moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-1");
        let reviewed_hit_key = moderation_hit_key(&session_key, &reviewed_text, &reviewed_hit);
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords,
            banned_sessions: Vec::new(),
            suppressed_hits: vec![ModerationSuppressedHit {
                session_key: session_key.clone(),
                hit_key: reviewed_hit_key,
                matched_keyword: reviewed_hit.keyword.clone(),
                match_start: reviewed_hit.match_start as i64,
                match_end: reviewed_hit.match_end as i64,
                match_prefix_sha256: moderation_match_prefix_sha256(
                    &reviewed_text,
                    reviewed_hit.match_start,
                ),
                keyword_set_hash,
            }],
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: b"{}",
                client_ip: "127.0.0.1",
            },
            || changed_text.clone(),
        );
        assert!(matches!(decision, ModerationDecision::Block { .. }));

        for _ in 0..20 {
            if !store.records().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let records = store.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].matched_keyword, "carding tutorial");
        assert_eq!(records[0].match_start, reviewed_hit.match_start as i64);
    }

    #[tokio::test]
    async fn unbanned_keyword_is_suppressed_later_in_same_session_even_when_context_moves() {
        let keywords =
            vec![moderation_keyword(1, "build a bomb"), moderation_keyword(2, "carding tutorial")];
        let keyword_set_hash = moderation_keyword_set_hash(&keywords);
        let reviewed_text = normalize_moderation_text("old false positive: build a bomb");
        let retry_text = normalize_moderation_text(
            "new prefix in the same session, still the same reviewed phrase: build a bomb",
        );
        let matcher = ModerationMatcher::build(
            keywords
                .iter()
                .map(|keyword| (keyword.keyword.as_str(), keyword.categories.clone())),
        )
        .expect("build matcher")
        .expect("matcher");
        let reviewed_hit = matcher.find(&reviewed_text).expect("reviewed hit");
        assert_eq!(reviewed_hit.keyword, "build a bomb");

        let session_key = moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-1");
        let reviewed_hit_key = moderation_hit_key(&session_key, &reviewed_text, &reviewed_hit);
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords,
            banned_sessions: Vec::new(),
            suppressed_hits: vec![ModerationSuppressedHit {
                session_key: session_key.clone(),
                hit_key: reviewed_hit_key,
                matched_keyword: reviewed_hit.keyword.clone(),
                match_start: reviewed_hit.match_start as i64,
                match_end: reviewed_hit.match_end as i64,
                match_prefix_sha256: moderation_match_prefix_sha256(
                    &reviewed_text,
                    reviewed_hit.match_start,
                ),
                keyword_set_hash,
            }],
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: b"{}",
                client_ip: "127.0.0.1",
            },
            || retry_text.clone(),
        );

        assert!(matches!(decision, ModerationDecision::Allow));
        assert!(store.records().is_empty());
    }

    #[tokio::test]
    async fn co_located_keyword_still_blocks_after_sibling_unbanned() {
        // "bomb" and "bomb making" hit the SAME start offset. Unbanning the
        // shorter one must not mask the distinct longer one (regression for the
        // suppression-advance-by-offset bypass).
        let keywords = vec![moderation_keyword(1, "bomb"), moderation_keyword(2, "bomb making")];
        let keyword_set_hash = moderation_keyword_set_hash(&keywords);
        let text = normalize_moderation_text("here is bomb making guide");
        let matcher = ModerationMatcher::build(
            keywords
                .iter()
                .map(|keyword| (keyword.keyword.as_str(), keyword.categories.clone())),
        )
        .expect("build matcher")
        .expect("matcher");
        // find() returns the shortest-end hit first: "bomb". Suppress exactly it.
        let bomb_hit = matcher.find(&text).expect("bomb hit");
        assert_eq!(bomb_hit.keyword, "bomb");

        let session_key = moderation_session_key(MODERATION_PROVIDER_KIRO, "key-1", "sess-1");
        let bomb_hit_key = moderation_hit_key(&session_key, &text, &bomb_hit);
        let store = Arc::new(MemoryModerationStore::with_snapshot(ModerationRuntimeSnapshot {
            keywords,
            banned_sessions: Vec::new(),
            suppressed_hits: vec![ModerationSuppressedHit {
                session_key: session_key.clone(),
                hit_key: bomb_hit_key.clone(),
                matched_keyword: bomb_hit.keyword.clone(),
                match_start: bomb_hit.match_start as i64,
                match_end: bomb_hit.match_end as i64,
                match_prefix_sha256: moderation_match_prefix_sha256(&text, bomb_hit.match_start),
                keyword_set_hash,
            }],
        }));
        let gate = ModerationGate::new(store.clone());
        gate.reload().await.expect("load moderation snapshot");

        let headers = HeaderMap::new();
        let key = sample_key();
        let decision = enforce_moderation(
            &gate,
            ModerationRequest {
                provider: MODERATION_PROVIDER_KIRO,
                key: &key,
                session_id: Some("sess-1"),
                endpoint: "/v1/messages",
                model: "claude-sonnet-4",
                headers: &headers,
                body: b"{}",
                client_ip: "127.0.0.1",
            },
            || text.clone(),
        );
        assert!(matches!(decision, ModerationDecision::Block { .. }));

        for _ in 0..20 {
            if !store.records().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let records = store.records();
        assert_eq!(records.len(), 1);
        // The distinct co-located keyword must be the one that blocks.
        assert_eq!(records[0].matched_keyword, "bomb making");
        assert_eq!(records[0].match_start, bomb_hit.match_start as i64);
        assert_ne!(records[0].hit_key, bomb_hit_key);
    }
}
