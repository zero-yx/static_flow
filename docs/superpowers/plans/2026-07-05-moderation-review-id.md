# Moderation Review Id Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Include a stable, admin-searchable moderation review id in every keyword-moderation block response, including repeat blocks that short-circuit before scanning.

**Architecture:** Reuse the existing `hit_key` as the public review id. Store the active ban cache as `session_key -> hit_key` so precheck can return the original active hit id without rescanning the body or exposing raw `session_key` / `key_id`.

**Tech Stack:** Rust, Axum, Yew/WASM, Postgres, existing `AdminModerationStore` abstraction.

---

## File Structure

- Modify `crates/llm-access-core/src/store/moderation.rs`
  - Add a compact `ModerationBannedSessionRef { session_key, hit_key }`.
  - Change runtime snapshot banned state from `Vec<String>` to `Vec<ModerationBannedSessionRef>`.
  - Add a banned-session admin query type for status + search.
- Modify `crates/llm-access-core/src/store/traits.rs`
  - Update `list_moderation_banned_sessions` to accept the query type.
- Modify `crates/llm-access-core/src/store/empty.rs`
  - Keep the empty store returning empty pages under the new signature.
- Modify `crates/llm-access-store/src/postgres/moderation.rs`
  - Load `session_key + hit_key` into the runtime snapshot.
  - Add search filtering for admin banned-session list.
- Modify `crates/llm-access-store/src/postgres.rs`
  - Update existing moderation store tests for `banned_sessions`.
  - Add search-by-`hit_key` coverage.
- Modify `crates/llm-access/src/moderation.rs`
  - Change runtime cache to `HashMap<String, String>`.
  - Return `ModerationDecision::Block { review_id }`.
  - Add `moderation_blocked_message(&review_id)`.
- Modify dispatch callers:
  - `crates/llm-access/src/provider/codex_dispatch.rs`
  - `crates/llm-access/src/provider/kiro_dispatch.rs`
  - `crates/llm-access/src/provider/anthropic_upstream_dispatch.rs`
- Modify admin API:
  - `crates/llm-access/src/admin.rs`
- Modify frontend:
  - `crates/frontend/src/api.rs`
  - `crates/frontend/src/pages/admin_moderation.rs`
  - Show `hit_key` directly as the visible review id in both the banned-session list and detail panel.

## Task 1: Runtime Cache Carries Review Id

**Files:**
- Modify: `crates/llm-access-core/src/store/moderation.rs`
- Modify: `crates/llm-access/src/moderation.rs`

- [ ] **Step 1: Write the failing moderation runtime test**

Add a test in `crates/llm-access/src/moderation.rs` near the existing moderation tests:

```rust
#[tokio::test]
async fn already_banned_session_returns_original_review_id_without_rescan() {
    let store = Arc::new(RecordingModerationStore::new(ModerationRuntimeSnapshot {
        keywords: vec![keyword("build a bomb")],
        banned_sessions: Vec::new(),
        suppressed_hits: Vec::new(),
    }));
    let gate = ModerationGate::new(store.clone());
    gate.reload().await.expect("load moderation snapshot");

    let key = test_key();
    let first = enforce_moderation(
        &gate,
        ModerationRequest {
            provider: MODERATION_PROVIDER_KIRO,
            key: &key,
            session_id: Some("sess-1"),
            endpoint: "/v1/messages",
            model: "claude",
            headers: &HeaderMap::new(),
            body: br#"{"messages":[{"role":"user","content":"build a bomb"}]}"#,
            client_ip: "127.0.0.1",
        },
        || "build a bomb".to_string(),
    );
    let ModerationDecision::Block { review_id } = first else {
        panic!("first keyword hit should block");
    };

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
            model: "claude",
            headers: &HeaderMap::new(),
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
```

- [ ] **Step 2: Run the focused red test**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access moderation::tests::already_banned_session_returns_original_review_id_without_rescan --jobs 4
```

Expected: FAIL because `ModerationRuntimeSnapshot` has no `banned_sessions` field and `ModerationDecision::Block` has no `review_id`.

- [ ] **Step 3: Add runtime snapshot ban references**

In `crates/llm-access-core/src/store/moderation.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationBannedSessionRef {
    /// Runtime ban key, not exposed to clients.
    pub session_key: String,
    /// Public-safe review id for the active hit.
    pub hit_key: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationRuntimeSnapshot {
    pub keywords: Vec<ModerationKeyword>,
    pub banned_sessions: Vec<ModerationBannedSessionRef>,
    pub suppressed_hits: Vec<ModerationSuppressedHit>,
}
```

- [ ] **Step 4: Change the in-memory gate cache**

In `crates/llm-access/src/moderation.rs`, replace `HashSet` banned state with `HashMap`:

```rust
use std::collections::{HashMap, HashSet};

pub(crate) enum ModerationPrecheck {
    Blocked { review_id: String },
    Skip,
    Scan(ModerationScanPlan),
}

#[derive(Default)]
struct ModerationGateState {
    matcher: Option<Arc<ModerationMatcher>>,
    keyword_count: usize,
    banned: HashMap<String, String>,
    suppressed_hit_keys: Arc<HashSet<String>>,
    keyword_set_hash: String,
    loaded: bool,
    loaded_at_ms: Option<i64>,
}
```

Update reload and precheck:

```rust
state.banned = snapshot
    .banned_sessions
    .into_iter()
    .map(|ban| (ban.session_key, ban.hit_key))
    .collect();

if let Some(session_key) = session_key {
    if let Some(review_id) = state.banned.get(session_key) {
        return ModerationPrecheck::Blocked {
            review_id: review_id.clone(),
        };
    }
}
```

Update state insertion:

```rust
impl ModerationGateState {
    fn state_ban(&mut self, session_key: &str, hit_key: &str) -> bool {
        self.banned
            .insert(session_key.to_string(), hit_key.to_string())
            .is_none()
    }
}
```

- [ ] **Step 5: Return review id from decisions**

In `crates/llm-access/src/moderation.rs`:

```rust
pub(crate) enum ModerationDecision {
    Allow,
    Block { review_id: String },
}

pub(crate) fn moderation_blocked_message(review_id: &str) -> String {
    format!(
        "{MODERATION_BLOCKED_MESSAGE} Moderation review id: {review_id}."
    )
}
```

In `enforce_moderation`:

```rust
ModerationPrecheck::Blocked { review_id } => {
    gate.note_blocked_request();
    ModerationDecision::Block { review_id }
}
```

For a fresh hit, clone `hit_key` before moving it into the record:

```rust
let review_id = hit_key.clone();
gate.ban_session(NewModerationBannedSession {
    hit_key,
    session_key,
    /* existing fields unchanged */
});
ModerationDecision::Block { review_id }
```

- [ ] **Step 6: Run the focused test**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access moderation::tests::already_banned_session_returns_original_review_id_without_rescan --jobs 4
```

Expected: PASS.

## Task 2: Provider Responses Include Review Id

**Files:**
- Modify: `crates/llm-access/src/provider/codex_dispatch.rs`
- Modify: `crates/llm-access/src/provider/kiro_dispatch.rs`
- Modify: `crates/llm-access/src/provider/anthropic_upstream_dispatch.rs`
- Modify: `crates/llm-access/src/moderation.rs`

- [ ] **Step 1: Add a message unit test**

In `crates/llm-access/src/moderation.rs`:

```rust
#[test]
fn moderation_blocked_message_includes_review_id() {
    let message = moderation_blocked_message("hit-review-123");
    assert!(message.contains(MODERATION_BLOCKED_MESSAGE));
    assert!(message.contains("hit-review-123"));
}
```

- [ ] **Step 2: Run the red test**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access moderation::tests::moderation_blocked_message_includes_review_id --jobs 4
```

Expected: FAIL until `moderation_blocked_message` exists.

- [ ] **Step 3: Update dispatch imports and matches**

Replace imports of `MODERATION_BLOCKED_MESSAGE` in the three dispatch files with `moderation_blocked_message`.

Codex:

```rust
if let ModerationDecision::Block { review_id } = enforce_moderation(/* existing args */) {
    usage_meta.mark_session_blocked();
    let message = moderation_blocked_message(&review_id);
    return codex_surface_error_response(&gateway_path, StatusCode::FORBIDDEN, &message);
}
```

Kiro native:

```rust
if let ModerationDecision::Block { review_id } = enforce_moderation(/* existing args */) {
    let message = moderation_blocked_message(&review_id);
    return kiro_json_error(StatusCode::FORBIDDEN, "permission_error", &message);
}
```

Direct Anthropic:

```rust
ModerationDecision::Block { review_id } => {
    let message = moderation_blocked_message(&review_id);
    Some(kiro_json_error(
        StatusCode::FORBIDDEN,
        "permission_error",
        &message,
    ))
}
```

- [ ] **Step 4: Run moderation tests**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access moderation::tests:: --jobs 4
```

Expected: PASS.

## Task 3: Store Snapshot and Admin Search

**Files:**
- Modify: `crates/llm-access-core/src/store/moderation.rs`
- Modify: `crates/llm-access-core/src/store/traits.rs`
- Modify: `crates/llm-access-core/src/store/empty.rs`
- Modify: `crates/llm-access-store/src/postgres/moderation.rs`
- Modify: `crates/llm-access-store/src/postgres.rs`
- Modify: `crates/llm-access/src/admin.rs`

- [ ] **Step 1: Add banned-session query type**

In `crates/llm-access-core/src/store/moderation.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminModerationBannedSessionPageQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
}
```

- [ ] **Step 2: Update trait signature**

In `crates/llm-access-core/src/store/traits.rs`:

```rust
async fn list_moderation_banned_sessions(
    &self,
    page: AdminPageRequest,
    query: &AdminModerationBannedSessionPageQuery,
) -> anyhow::Result<ModerationBannedSessionsPage>;
```

- [ ] **Step 3: Load active hit refs in Postgres snapshot**

In `crates/llm-access-store/src/postgres/moderation.rs`:

```rust
let banned_rows = self
    .client
    .query(
        "SELECT DISTINCT ON (session_key) session_key, hit_key
         FROM llm_moderation_banned_sessions
         WHERE status = 'banned'
         ORDER BY session_key, banned_at_ms ASC, id ASC",
        &[],
    )
    .await
    .context("load postgres moderation banned session refs")?;
let banned_sessions = banned_rows
    .iter()
    .map(|row| ModerationBannedSessionRef {
        session_key: row.get(0),
        hit_key: row.get(1),
    })
    .collect();
```

- [ ] **Step 4: Add Postgres admin search**

Search should match `hit_key`, `session_key`, `session_id`, `key_id`, `key_name`, `matched_keyword`, `matched_context`, `endpoint`, `model`, `client_ip`, and `review_note`.

Use a helper that builds two cases: status-only and status+search. Keep limit/offset parameterized.

```rust
fn normalized_banned_session_search(search: Option<&str>) -> Option<String> {
    search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
```

Use SQL predicates like:

```sql
(
    hit_key ILIKE $search
    OR session_key ILIKE $search
    OR session_id ILIKE $search
    OR key_id ILIKE $search
    OR key_name ILIKE $search
    OR matched_keyword ILIKE $search
    OR matched_context ILIKE $search
    OR endpoint ILIKE $search
    OR model ILIKE $search
    OR client_ip ILIKE $search
    OR COALESCE(review_note, '') ILIKE $search
)
```

- [ ] **Step 5: Add store test coverage**

In `crates/llm-access-store/src/postgres.rs`, after the existing banned-session list assertions:

```rust
let hit_search = repo
    .list_moderation_banned_sessions(
        AdminPageRequest { limit: 10, offset: 0 },
        &AdminModerationBannedSessionPageQuery {
            status: Some("all".to_string()),
            search: Some("hit-1".to_string()),
        },
    )
    .await
    .expect("search banned sessions by hit key");
assert_eq!(hit_search.total, 1);
assert_eq!(hit_search.sessions[0].hit_key, "hit-1");
```

Update snapshot assertion:

```rust
let mut banned_sessions = snapshot
    .banned_sessions
    .into_iter()
    .map(|ban| (ban.session_key, ban.hit_key))
    .collect::<Vec<_>>();
banned_sessions.sort();
assert_eq!(
    banned_sessions,
    vec![("codex:key-1:sess-2".to_string(), "hit-2".to_string())]
);
```

- [ ] **Step 6: Update admin API query**

In `crates/llm-access/src/admin.rs`:

```rust
#[derive(Debug, Deserialize, Default)]
pub(crate) struct AdminModerationBannedSessionsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    status: Option<String>,
    #[serde(alias = "search")]
    q: Option<String>,
}
```

Call the store with:

```rust
let query = core_store::AdminModerationBannedSessionPageQuery {
    status: status.map(str::to_string),
    search: query.q.clone(),
};
state
    .admin_moderation_store
    .list_moderation_banned_sessions(page_request, &query)
    .await
```

- [ ] **Step 7: Run store/admin-focused tests**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access-store postgres::tests::postgres_repository_manages_moderation_keywords_and_bans --jobs 4
cargo test -p llm-access moderation::tests:: --jobs 4
```

Expected: PASS.

## Task 4: Frontend Search and Hit Key Visibility

**Files:**
- Modify: `crates/frontend/src/api.rs`
- Modify: `crates/frontend/src/pages/admin_moderation.rs`

- [ ] **Step 1: Extend frontend API function**

In `crates/frontend/src/api.rs`:

```rust
pub async fn fetch_admin_moderation_banned_sessions(
    status: &str,
    search: &str,
    limit: usize,
    offset: usize,
) -> Result<AdminModerationBannedSessionsResponse, String> {
    let mut url = format!(
        "{}/admin/llm-gateway/moderation/banned-sessions?status={}&limit={limit}&offset={offset}",
        llm_access_admin_base(),
        urlencoding::encode(status)
    );
    let search = search.trim();
    if !search.is_empty() {
        url.push_str("&q=");
        url.push_str(&urlencoding::encode(search));
    }
    /* existing request handling unchanged */
}
```

- [ ] **Step 2: Add UI state and loading dependency**

In `crates/frontend/src/pages/admin_moderation.rs`:

```rust
let session_search = use_state(String::new);
```

Use it in the effect dependency and request:

```rust
let search = (*session_search).clone();
let deps = (*refresh_tick, status.clone(), search.clone(), offset);
use_effect_with(deps, move |_| {
    /* ... */
    match fetch_admin_moderation_banned_sessions(
        &status,
        &search,
        SESSIONS_PAGE_SIZE,
        offset,
    )
    .await
    {
        /* existing handling */
    }
});
```

- [ ] **Step 3: Add search controls and visible hit_key column**

Add a search input near the status filter:

```rust
<input
    type="search"
    class={classes!("min-w-[18rem]", "rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
    placeholder="Search review id, session, key, keyword..."
    value={(*session_search).clone()}
    oninput={on_session_search_input}
/>
```

Reset paging on search:

```rust
let on_session_search_input = {
    let session_search = session_search.clone();
    let session_offset = session_offset.clone();
    Callback::from(move |e: InputEvent| {
        let target: HtmlInputElement = e.target_unchecked_into();
        session_search.set(target.value());
        session_offset.set(0);
    })
};
```

Add a table header and cell. This must render the actual `hit_key` value so an
admin can visually match the user-provided review id to the stored hit:

```rust
<th scope="col" class={classes!("px-4", "py-2")}>{ "Hit key" }</th>
```

```rust
<td class={classes!("px-4", "py-2", "font-mono", "text-[10px]", "break-all")}>
    { &session.hit_key }
</td>
```

In the detail panel, show the same full `hit_key` under a `Hit key` label before
the raw `session_key`. Do not truncate it in the detail panel; the operator must
be able to compare it directly with the id a user provides.

- [ ] **Step 4: Check frontend compile**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo check -p static-flow-frontend --target wasm32-unknown-unknown --jobs 4
```

Expected: PASS.

## Task 5: Formatting and Final Verification

**Files:**
- All modified Rust files from tasks above.

- [ ] **Step 1: Confirm submodules are clean before formatting**

```bash
git -C deps/lance status --short
git -C deps/lancedb status --short
```

Expected: no unrelated dirty output caused by this task.

- [ ] **Step 2: Format exact changed Rust files only**

```bash
rustfmt \
  crates/llm-access-core/src/store/moderation.rs \
  crates/llm-access-core/src/store/traits.rs \
  crates/llm-access-core/src/store/empty.rs \
  crates/llm-access-store/src/postgres/moderation.rs \
  crates/llm-access-store/src/postgres.rs \
  crates/llm-access/src/moderation.rs \
  crates/llm-access/src/admin.rs \
  crates/llm-access/src/provider/codex_dispatch.rs \
  crates/llm-access/src/provider/kiro_dispatch.rs \
  crates/llm-access/src/provider/anthropic_upstream_dispatch.rs \
  crates/frontend/src/api.rs \
  crates/frontend/src/pages/admin_moderation.rs
```

- [ ] **Step 3: Run affected tests and clippy**

```bash
export CARGO_TARGET_DIR=/mnt/wsl/data4tb/static-flow-data/cargo-target/static_flow
pgrep -af 'cargo|rustc|trunk|ld|lld|mold'
cargo test -p llm-access-core store::moderation::tests:: --jobs 4
cargo test -p llm-access-store postgres::tests::postgres_repository_manages_moderation_keywords_and_bans --jobs 4
cargo test -p llm-access moderation::tests:: --jobs 4
cargo check -p static-flow-frontend --target wasm32-unknown-unknown --jobs 4
cargo clippy -p llm-access-core -p llm-access-store -p llm-access --jobs 4 -- -D warnings
cargo clippy -p static-flow-frontend --target wasm32-unknown-unknown --jobs 4 -- -D warnings
git diff --check
```

Expected: all commands pass.

## Self-Review

- Spec coverage: every block path returns a review id, including precheck; admin can paste the id into search; the same `hit_key` is visibly rendered in the list and detail UI; raw `session_key` remains absent from user-facing errors.
- Backward compatibility: no DB migration is required because `hit_key` already exists and is already persisted.
- Performance: repeat-block fast path stays O(1) in memory and does not parse or scan the body.
- Security: user-facing error exposes `hit_key`, not raw `provider:key_id:session_id`.
- Residual risk: if one session has multiple still-banned hits, repeat-block responses point to the deterministic active hit loaded into `session_key -> hit_key`, not to a fresh scan of the blocked request.
