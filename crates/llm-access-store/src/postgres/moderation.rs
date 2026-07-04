//! Keyword moderation store: configured keywords plus banned sessions with
//! their captured request payloads.

use anyhow::Context;
use async_trait::async_trait;
use llm_access_core::store::{
    AdminModerationBannedSessionPageQuery, AdminModerationKeywordPageQuery, AdminModerationStore,
    AdminPageRequest, ModerationBannedSession, ModerationBannedSessionDetail,
    ModerationBannedSessionRef, ModerationBannedSessionsPage, ModerationCategory,
    ModerationKeyword, ModerationKeywordImportOutcome, ModerationKeywordsPage,
    ModerationRuntimeSnapshot, ModerationSuppressedHit, NewModerationBannedSession,
    NewModerationCategory, NewModerationKeyword, MODERATION_SESSION_STATUS_BANNED,
    MODERATION_SESSION_STATUS_UNBANNED,
};

use super::{now_ms, PgRow, PostgresControlRepository};

const MODERATION_CATEGORY_COLUMNS: &str = "code, label, description, severity, created_at_ms";

const MODERATION_KEYWORD_COLUMNS: &str =
    "id, keyword, note, source, created_at_ms, category_codes::text";

const MODERATION_BANNED_SESSION_COLUMNS: &str =
    "id, hit_key, session_key, provider, key_id, key_name, session_id, matched_keyword, \
     matched_context, match_start, match_end, match_prefix_sha256, keyword_set_hash, endpoint, \
     model, client_ip, status, review_note, banned_at_ms, reviewed_at_ms, updated_at_ms, \
     matched_categories::text";

/// Decode a JSONB text-array column (e.g. `["csam","cyber"]`) into codes. A
/// malformed value degrades to an empty list rather than failing the row.
fn category_codes_from_json_text(raw: String) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
}

fn category_codes_to_json(codes: &[String]) -> String {
    serde_json::to_string(codes).unwrap_or_else(|_| "[]".to_string())
}

fn moderation_category_from_row(row: &PgRow) -> ModerationCategory {
    ModerationCategory {
        code: row.get(0),
        label: row.get(1),
        description: row.get(2),
        severity: row.get(3),
        created_at_ms: row.get(4),
    }
}

fn moderation_keyword_from_row(row: &PgRow) -> ModerationKeyword {
    ModerationKeyword {
        id: row.get(0),
        keyword: row.get(1),
        note: row.get(2),
        source: row.get(3),
        created_at_ms: row.get(4),
        categories: category_codes_from_json_text(row.get(5)),
    }
}

fn moderation_banned_session_from_row(row: &PgRow) -> ModerationBannedSession {
    ModerationBannedSession {
        id: row.get(0),
        hit_key: row.get(1),
        session_key: row.get(2),
        provider: row.get(3),
        key_id: row.get(4),
        key_name: row.get(5),
        session_id: row.get(6),
        matched_keyword: row.get(7),
        matched_context: row.get(8),
        match_start: row.get(9),
        match_end: row.get(10),
        match_prefix_sha256: row.get(11),
        keyword_set_hash: row.get(12),
        endpoint: row.get(13),
        model: row.get(14),
        client_ip: row.get(15),
        status: row.get(16),
        review_note: row.get(17),
        banned_at_ms: row.get(18),
        reviewed_at_ms: row.get(19),
        updated_at_ms: row.get(20),
        matched_categories: category_codes_from_json_text(row.get(21)),
    }
}

fn normalized_session_status_filter(status: Option<&str>) -> anyhow::Result<Option<&str>> {
    match status.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some("all") => Ok(None),
        Some(MODERATION_SESSION_STATUS_BANNED) => Ok(Some(MODERATION_SESSION_STATUS_BANNED)),
        Some(MODERATION_SESSION_STATUS_UNBANNED) => Ok(Some(MODERATION_SESSION_STATUS_UNBANNED)),
        Some(other) => anyhow::bail!("unsupported moderation session status filter `{other}`"),
    }
}

fn normalized_banned_session_search_filter(search: Option<&str>) -> Option<String> {
    search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("%{value}%"))
}

fn normalized_keyword_search_filter(search: Option<&str>) -> Option<String> {
    search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase)
}

fn compact_keyword_search_filter(search: &str) -> String {
    search
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[async_trait]
impl AdminModerationStore for PostgresControlRepository {
    async fn load_moderation_runtime_snapshot(&self) -> anyhow::Result<ModerationRuntimeSnapshot> {
        self.ensure_connection_alive()?;
        let keywords = self.list_moderation_keywords().await?;
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
        let suppressed_rows = self
            .client
            .query(
                "SELECT session_key, hit_key, match_start, match_end, match_prefix_sha256,
                        keyword_set_hash
                 FROM llm_moderation_banned_sessions
                 WHERE status = 'unbanned'",
                &[],
            )
            .await
            .context("load postgres moderation suppressed hit keys")?;
        let suppressed_hits = suppressed_rows
            .iter()
            .map(|row| ModerationSuppressedHit {
                session_key: row.get(0),
                hit_key: row.get(1),
                match_start: row.get(2),
                match_end: row.get(3),
                match_prefix_sha256: row.get(4),
                keyword_set_hash: row.get(5),
            })
            .collect();
        Ok(ModerationRuntimeSnapshot {
            keywords,
            banned_sessions,
            suppressed_hits,
        })
    }

    async fn list_moderation_categories(&self) -> anyhow::Result<Vec<ModerationCategory>> {
        self.ensure_connection_alive()?;
        let sql = format!(
            "SELECT {MODERATION_CATEGORY_COLUMNS} FROM llm_moderation_categories ORDER BY code"
        );
        let rows = self
            .client
            .query(&sql, &[])
            .await
            .context("list postgres moderation categories")?;
        Ok(rows.iter().map(moderation_category_from_row).collect())
    }

    async fn add_moderation_categories(
        &self,
        categories: Vec<NewModerationCategory>,
    ) -> anyhow::Result<usize> {
        self.ensure_connection_alive()?;
        if categories.is_empty() {
            return Ok(0);
        }
        let total = categories.len();
        let mut code_values = Vec::with_capacity(total);
        let mut label_values = Vec::with_capacity(total);
        let mut description_values = Vec::with_capacity(total);
        let mut severity_values = Vec::with_capacity(total);
        let mut created_at_values = Vec::with_capacity(total);
        for category in categories {
            code_values.push(category.code);
            label_values.push(category.label);
            description_values.push(category.description);
            severity_values.push(category.severity);
            created_at_values.push(category.created_at_ms);
        }
        let inserted = self
            .client
            .execute(
                "INSERT INTO llm_moderation_categories (code, label, description, severity, \
                 created_at_ms)
                 SELECT code, label, description, severity, created_at_ms
                 FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[])
                     AS input(code, label, description, severity, created_at_ms)
                 ON CONFLICT (code) DO NOTHING",
                &[
                    &code_values,
                    &label_values,
                    &description_values,
                    &severity_values,
                    &created_at_values,
                ],
            )
            .await
            .context("insert postgres moderation categories")?;
        Ok(inserted as usize)
    }

    async fn delete_moderation_category(
        &self,
        code: &str,
    ) -> anyhow::Result<Option<ModerationCategory>> {
        self.ensure_connection_alive()?;
        // Refuse deletion while any keyword still references the category, so a
        // keyword can never point at a dangling code.
        let referenced: bool = self
            .client
            .query_one(
                "SELECT EXISTS(
                    SELECT 1 FROM llm_moderation_keywords WHERE category_codes ? $1
                )",
                &[&code],
            )
            .await
            .context("check postgres moderation category references")?
            .get(0);
        if referenced {
            anyhow::bail!("category `{code}` is still referenced by one or more keywords");
        }
        let sql = format!(
            "DELETE FROM llm_moderation_categories WHERE code = $1
             RETURNING {MODERATION_CATEGORY_COLUMNS}"
        );
        let row = self
            .client
            .query_opt(&sql, &[&code])
            .await
            .context("delete postgres moderation category")?;
        Ok(row.as_ref().map(moderation_category_from_row))
    }

    async fn list_moderation_keywords(&self) -> anyhow::Result<Vec<ModerationKeyword>> {
        self.ensure_connection_alive()?;
        let sql = format!(
            "SELECT {MODERATION_KEYWORD_COLUMNS} FROM llm_moderation_keywords ORDER BY id DESC"
        );
        let rows = self
            .client
            .query(&sql, &[])
            .await
            .context("list postgres moderation keywords")?;
        Ok(rows.iter().map(moderation_keyword_from_row).collect())
    }

    async fn list_moderation_keywords_page(
        &self,
        page: AdminPageRequest,
        query: &AdminModerationKeywordPageQuery,
    ) -> anyhow::Result<ModerationKeywordsPage> {
        self.ensure_connection_alive()?;
        let limit = page.limit.max(1);
        let limit_i64 = limit.min(i64::MAX as usize) as i64;
        let offset_i64 = page.offset.min(i64::MAX as usize) as i64;
        let search = normalized_keyword_search_filter(query.search.as_deref());
        let (total, rows) = match search {
            Some(search) => {
                let pattern = format!("%{search}%");
                let compact_pattern = format!("%{}%", compact_keyword_search_filter(&search));
                let where_clause = "keyword ILIKE $1
                    OR replace(keyword, ' ', '') ILIKE $2
                    OR COALESCE(note, '') ILIKE $1
                    OR source ILIKE $1
                    OR category_codes::text ILIKE $1";
                let total: i64 = self
                    .client
                    .query_one(
                        &format!(
                            "SELECT COUNT(*) FROM llm_moderation_keywords WHERE {where_clause}"
                        ),
                        &[&pattern, &compact_pattern],
                    )
                    .await
                    .context("count postgres moderation keywords")?
                    .get(0);
                let sql = format!(
                    "SELECT {MODERATION_KEYWORD_COLUMNS}
                     FROM llm_moderation_keywords
                     WHERE {where_clause}
                     ORDER BY id DESC
                     LIMIT $3 OFFSET $4"
                );
                let rows = self
                    .client
                    .query(&sql, &[&pattern, &compact_pattern, &limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation keywords page")?;
                (total, rows)
            },
            None => {
                let total: i64 = self
                    .client
                    .query_one("SELECT COUNT(*) FROM llm_moderation_keywords", &[])
                    .await
                    .context("count postgres moderation keywords")?
                    .get(0);
                let sql = format!(
                    "SELECT {MODERATION_KEYWORD_COLUMNS}
                     FROM llm_moderation_keywords
                     ORDER BY id DESC
                     LIMIT $1 OFFSET $2"
                );
                let rows = self
                    .client
                    .query(&sql, &[&limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation keywords page")?;
                (total, rows)
            },
        };
        let keywords: Vec<ModerationKeyword> =
            rows.iter().map(moderation_keyword_from_row).collect();
        let total = total.max(0) as usize;
        Ok(ModerationKeywordsPage {
            has_more: page.offset.saturating_add(keywords.len()) < total,
            keywords,
            total,
            limit,
            offset: page.offset,
        })
    }

    async fn add_moderation_keywords(
        &self,
        keywords: Vec<NewModerationKeyword>,
    ) -> anyhow::Result<ModerationKeywordImportOutcome> {
        self.ensure_connection_alive()?;
        if keywords.is_empty() {
            return Ok(ModerationKeywordImportOutcome::default());
        }
        let total = keywords.len();
        let mut keyword_values = Vec::with_capacity(total);
        let mut note_values = Vec::with_capacity(total);
        let mut source_values = Vec::with_capacity(total);
        let mut category_values = Vec::with_capacity(total);
        let mut created_at_values = Vec::with_capacity(total);
        for keyword in keywords {
            keyword_values.push(keyword.keyword);
            note_values.push(keyword.note.unwrap_or_default());
            source_values.push(keyword.source);
            category_values.push(category_codes_to_json(&keyword.categories));
            created_at_values.push(keyword.created_at_ms);
        }
        let inserted = self
            .client
            .execute(
                "INSERT INTO llm_moderation_keywords (keyword, note, source, category_codes, \
                 created_at_ms)
                 SELECT keyword, NULLIF(note, ''), source, category_codes::jsonb, created_at_ms
                 FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[])
                     AS input(keyword, note, source, category_codes, created_at_ms)
                 ON CONFLICT (keyword) DO NOTHING",
                &[
                    &keyword_values,
                    &note_values,
                    &source_values,
                    &category_values,
                    &created_at_values,
                ],
            )
            .await
            .context("insert postgres moderation keywords")? as usize;
        Ok(ModerationKeywordImportOutcome {
            inserted,
            duplicates: total.saturating_sub(inserted),
        })
    }

    async fn delete_moderation_keyword(
        &self,
        id: i64,
    ) -> anyhow::Result<Option<ModerationKeyword>> {
        self.ensure_connection_alive()?;
        let sql = format!(
            "DELETE FROM llm_moderation_keywords WHERE id = $1
             RETURNING {MODERATION_KEYWORD_COLUMNS}"
        );
        let row = self
            .client
            .query_opt(&sql, &[&id])
            .await
            .context("delete postgres moderation keyword")?;
        Ok(row.as_ref().map(moderation_keyword_from_row))
    }

    async fn record_moderation_banned_session(
        &self,
        record: NewModerationBannedSession,
    ) -> anyhow::Result<bool> {
        self.ensure_connection_alive()?;
        let inserted = self
            .client
            .execute(
                "INSERT INTO llm_moderation_banned_sessions (
                    hit_key,
                    session_key,
                    provider,
                    key_id,
                    key_name,
                    session_id,
                    matched_keyword,
                    matched_context,
                    match_start,
                    match_end,
                    match_prefix_sha256,
                    keyword_set_hash,
                    endpoint,
                    model,
                    client_ip,
                    request_headers_json,
                    request_body_json,
                    matched_categories,
                    status,
                    banned_at_ms,
                    updated_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                         $13, $14, $15, $16, $17, $18::jsonb, 'banned', $19, $19)
                 ON CONFLICT (hit_key) DO NOTHING",
                &[
                    &record.hit_key,
                    &record.session_key,
                    &record.provider,
                    &record.key_id,
                    &record.key_name,
                    &record.session_id,
                    &record.matched_keyword,
                    &record.matched_context,
                    &record.match_start,
                    &record.match_end,
                    &record.match_prefix_sha256,
                    &record.keyword_set_hash,
                    &record.endpoint,
                    &record.model,
                    &record.client_ip,
                    &record.request_headers_json,
                    &record.request_body_json,
                    &category_codes_to_json(&record.matched_categories),
                    &record.banned_at_ms,
                ],
            )
            .await
            .context("record postgres moderation banned session")?;
        Ok(inserted > 0)
    }

    async fn list_moderation_banned_sessions(
        &self,
        page: AdminPageRequest,
        query: &AdminModerationBannedSessionPageQuery,
    ) -> anyhow::Result<ModerationBannedSessionsPage> {
        self.ensure_connection_alive()?;
        let status = normalized_session_status_filter(query.status.as_deref())?;
        let search = normalized_banned_session_search_filter(query.search.as_deref());
        let limit = page.limit.max(1);
        let limit_i64 = limit.min(i64::MAX as usize) as i64;
        let offset_i64 = page.offset.min(i64::MAX as usize) as i64;
        let search_clause = "(hit_key ILIKE $SEARCH
                         OR session_key ILIKE $SEARCH
                         OR session_id ILIKE $SEARCH
                         OR key_id ILIKE $SEARCH
                         OR key_name ILIKE $SEARCH
                         OR matched_keyword ILIKE $SEARCH
                         OR matched_context ILIKE $SEARCH
                         OR endpoint ILIKE $SEARCH
                         OR model ILIKE $SEARCH
                         OR client_ip ILIKE $SEARCH
                         OR COALESCE(review_note, '') ILIKE $SEARCH
                         OR matched_categories::text ILIKE $SEARCH)";
        let (total, rows) = match (status, search.as_deref()) {
            (Some(status), Some(search)) => {
                let total_sql = search_clause.replace("$SEARCH", "$2");
                let total: i64 = self
                    .client
                    .query_one(
                        &format!(
                            "SELECT COUNT(*) FROM llm_moderation_banned_sessions
                             WHERE status = $1 AND {total_sql}"
                        ),
                        &[&status, &search],
                    )
                    .await
                    .context("count postgres moderation banned sessions")?
                    .get(0);
                let list_sql = search_clause.replace("$SEARCH", "$2");
                let sql = format!(
                    "SELECT {MODERATION_BANNED_SESSION_COLUMNS}
                     FROM llm_moderation_banned_sessions
                     WHERE status = $1 AND {list_sql}
                     ORDER BY banned_at_ms DESC, id DESC
                     LIMIT $3 OFFSET $4"
                );
                let rows = self
                    .client
                    .query(&sql, &[&status, &search, &limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation banned sessions")?;
                (total, rows)
            },
            (Some(status), None) => {
                let total: i64 = self
                    .client
                    .query_one(
                        "SELECT COUNT(*) FROM llm_moderation_banned_sessions WHERE status = $1",
                        &[&status],
                    )
                    .await
                    .context("count postgres moderation banned sessions")?
                    .get(0);
                let sql = format!(
                    "SELECT {MODERATION_BANNED_SESSION_COLUMNS}
                     FROM llm_moderation_banned_sessions
                     WHERE status = $1
                     ORDER BY banned_at_ms DESC, id DESC
                     LIMIT $2 OFFSET $3"
                );
                let rows = self
                    .client
                    .query(&sql, &[&status, &limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation banned sessions")?;
                (total, rows)
            },
            (None, Some(search)) => {
                let total_sql = search_clause.replace("$SEARCH", "$1");
                let total: i64 = self
                    .client
                    .query_one(
                        &format!(
                            "SELECT COUNT(*) FROM llm_moderation_banned_sessions
                             WHERE {total_sql}"
                        ),
                        &[&search],
                    )
                    .await
                    .context("count postgres moderation banned sessions")?
                    .get(0);
                let list_sql = search_clause.replace("$SEARCH", "$1");
                let sql = format!(
                    "SELECT {MODERATION_BANNED_SESSION_COLUMNS}
                     FROM llm_moderation_banned_sessions
                     WHERE {list_sql}
                     ORDER BY banned_at_ms DESC, id DESC
                     LIMIT $2 OFFSET $3"
                );
                let rows = self
                    .client
                    .query(&sql, &[&search, &limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation banned sessions")?;
                (total, rows)
            },
            (None, None) => {
                let total: i64 = self
                    .client
                    .query_one("SELECT COUNT(*) FROM llm_moderation_banned_sessions", &[])
                    .await
                    .context("count postgres moderation banned sessions")?
                    .get(0);
                let sql = format!(
                    "SELECT {MODERATION_BANNED_SESSION_COLUMNS}
                     FROM llm_moderation_banned_sessions
                     ORDER BY banned_at_ms DESC, id DESC
                     LIMIT $1 OFFSET $2"
                );
                let rows = self
                    .client
                    .query(&sql, &[&limit_i64, &offset_i64])
                    .await
                    .context("list postgres moderation banned sessions")?;
                (total, rows)
            },
        };
        let sessions: Vec<ModerationBannedSession> = rows
            .iter()
            .map(moderation_banned_session_from_row)
            .collect();
        let total = total.max(0) as usize;
        Ok(ModerationBannedSessionsPage {
            has_more: page.offset.saturating_add(sessions.len()) < total,
            sessions,
            total,
            limit,
            offset: page.offset,
        })
    }

    async fn get_moderation_banned_session(
        &self,
        id: i64,
    ) -> anyhow::Result<Option<ModerationBannedSessionDetail>> {
        self.ensure_connection_alive()?;
        let sql = format!(
            "SELECT {MODERATION_BANNED_SESSION_COLUMNS},
                    request_headers_json,
                    request_body_json
             FROM llm_moderation_banned_sessions
             WHERE id = $1"
        );
        let Some(row) = self
            .client
            .query_opt(&sql, &[&id])
            .await
            .context("load postgres moderation banned session")?
        else {
            return Ok(None);
        };
        Ok(Some(ModerationBannedSessionDetail {
            session: moderation_banned_session_from_row(&row),
            request_headers_json: row.get(22),
            request_body_json: row.get(23),
        }))
    }

    async fn set_moderation_banned_session_status(
        &self,
        id: i64,
        status: &str,
        review_note: Option<&str>,
        reviewed_at_ms: i64,
    ) -> anyhow::Result<Option<ModerationBannedSession>> {
        self.ensure_connection_alive()?;
        if status != MODERATION_SESSION_STATUS_BANNED
            && status != MODERATION_SESSION_STATUS_UNBANNED
        {
            anyhow::bail!("unsupported moderation session status `{status}`");
        }
        let review_note = review_note
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let sql = format!(
            "UPDATE llm_moderation_banned_sessions
             SET status = $2,
                 review_note = COALESCE($3, review_note),
                 reviewed_at_ms = $4,
                 updated_at_ms = $5
             WHERE id = $1
             RETURNING {MODERATION_BANNED_SESSION_COLUMNS}"
        );
        let row = self
            .client
            .query_opt(&sql, &[&id, &status, &review_note, &reviewed_at_ms, &now_ms()])
            .await
            .context("update postgres moderation banned session status")?;
        Ok(row.as_ref().map(moderation_banned_session_from_row))
    }
}
