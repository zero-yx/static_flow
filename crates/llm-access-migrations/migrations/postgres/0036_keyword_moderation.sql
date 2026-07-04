-- Keyword moderation, durable control-plane state.
--
-- The request hot path never reads these tables directly: the llm-access
-- `moderation` gate loads a compact snapshot into process memory on startup /
-- periodic refresh / admin change, and writes here only when a NEW session is
-- banned. See crates/llm-access/src/moderation.rs for the caching contract.
--
--   llm_moderation_keywords         -- one row per banned phrase (canonical,
--                                      already tokenized; UNIQUE for dedup)
--   llm_moderation_banned_sessions  -- one row per banned hit, with the
--                                      captured request body + headers (TEXT,
--                                      verbatim) for reviewer inspection

CREATE TABLE IF NOT EXISTS llm_moderation_keywords (
    id BIGSERIAL PRIMARY KEY,
    keyword TEXT NOT NULL UNIQUE CHECK (length(keyword) > 0),
    note TEXT,
    source TEXT NOT NULL DEFAULT 'txt' CHECK (source IN ('txt', 'json')),
    created_at_ms BIGINT NOT NULL CHECK (created_at_ms >= 0)
);

CREATE TABLE IF NOT EXISTS llm_moderation_banned_sessions (
    id BIGSERIAL PRIMARY KEY,
    hit_key TEXT NOT NULL UNIQUE CHECK (length(hit_key) > 0),
    session_key TEXT NOT NULL CHECK (length(session_key) > 0),
    provider TEXT NOT NULL,
    key_id TEXT NOT NULL,
    key_name TEXT NOT NULL DEFAULT '',
    session_id TEXT NOT NULL DEFAULT '',
    matched_keyword TEXT NOT NULL,
    matched_context TEXT NOT NULL DEFAULT '',
    match_start BIGINT NOT NULL CHECK (match_start >= 0),
    match_end BIGINT NOT NULL CHECK (match_end >= match_start),
    match_prefix_sha256 TEXT NOT NULL DEFAULT '',
    keyword_set_hash TEXT NOT NULL DEFAULT '',
    endpoint TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    client_ip TEXT NOT NULL DEFAULT '',
    -- Captured verbatim as TEXT (not JSONB) so the exact request bytes are
    -- preserved for review rather than reparsed/reordered through JSONB.
    request_headers_json TEXT NOT NULL DEFAULT '{}',
    request_body_json TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'banned' CHECK (status IN ('banned', 'unbanned')),
    review_note TEXT,
    banned_at_ms BIGINT NOT NULL CHECK (banned_at_ms >= 0),
    reviewed_at_ms BIGINT CHECK (reviewed_at_ms IS NULL OR reviewed_at_ms >= 0),
    updated_at_ms BIGINT NOT NULL CHECK (updated_at_ms >= 0)
);

CREATE INDEX IF NOT EXISTS idx_llm_moderation_banned_sessions_status_banned_at
    ON llm_moderation_banned_sessions(status, banned_at_ms DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_llm_moderation_banned_sessions_banned_at
    ON llm_moderation_banned_sessions(banned_at_ms DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_llm_moderation_banned_sessions_key_id
    ON llm_moderation_banned_sessions(key_id, banned_at_ms DESC);

-- A session may hold several ban rows over time (one per distinct keyword hit,
-- as content is reviewed and later content re-scanned). Uniqueness is per hit
-- (hit_key UNIQUE); the runtime derives per-session banned state via
-- SELECT DISTINCT session_key WHERE status = 'banned'. There is deliberately no
-- unique constraint on session_key.
