//! Keyword moderation data model: configured keywords and banned sessions.

use serde::{Deserialize, Serialize};

use super::AdminPageRequest;

/// Ban record status: the session is actively blocked.
pub const MODERATION_SESSION_STATUS_BANNED: &str = "banned";
/// Ban record status: a reviewer cleared this hit; later scans skip this hit
/// position, not the whole session.
pub const MODERATION_SESSION_STATUS_UNBANNED: &str = "unbanned";

/// Keyword import source label for plain-text payloads.
pub const MODERATION_KEYWORD_SOURCE_TXT: &str = "txt";
/// Keyword import source label for JSON payloads.
pub const MODERATION_KEYWORD_SOURCE_JSON: &str = "json";

/// Category severity: reserved for the highest-harm categories (CSAM, weapons,
/// self-harm, …).
pub const MODERATION_CATEGORY_SEVERITY_CRITICAL: &str = "critical";
/// Category severity default.
pub const MODERATION_CATEGORY_SEVERITY_MEDIUM: &str = "medium";

/// One risk category a keyword can be classified under. Categories are managed
/// separately from keywords (see
/// [`AdminModerationStore`](super::AdminModerationStore)), and a keyword may
/// reference several by [`code`](ModerationCategory::code).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationCategory {
    /// Stable machine code, e.g. `csam`, `cyber`. Referenced by keywords.
    pub code: String,
    /// Human-facing label (may be bilingual).
    pub label: String,
    /// Longer description shown in the admin console.
    pub description: String,
    /// `critical` / `high` / `medium` / `low`.
    pub severity: String,
    /// Creation timestamp.
    pub created_at_ms: i64,
}

/// Insert payload for one risk category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewModerationCategory {
    /// Stable machine code.
    pub code: String,
    /// Human-facing label.
    pub label: String,
    /// Longer description.
    pub description: String,
    /// Severity bucket.
    pub severity: String,
    /// Creation timestamp.
    pub created_at_ms: i64,
}

/// One configured moderation keyword.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationKeyword {
    /// Stable row id.
    pub id: i64,
    /// Normalized keyword phrase (lowercased, single-space separated).
    pub keyword: String,
    /// Risk-category codes this keyword is classified under (may be several).
    #[serde(default)]
    pub categories: Vec<String>,
    /// Optional reviewer note.
    pub note: Option<String>,
    /// Import source: `txt` or `json`.
    pub source: String,
    /// Creation timestamp.
    pub created_at_ms: i64,
}

/// Insert payload for one moderation keyword.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewModerationKeyword {
    /// Normalized keyword phrase.
    pub keyword: String,
    /// Risk-category codes to classify this keyword under.
    pub categories: Vec<String>,
    /// Optional reviewer note.
    pub note: Option<String>,
    /// Import source: `txt` or `json`.
    pub source: String,
    /// Creation timestamp.
    pub created_at_ms: i64,
}

/// Outcome of one bulk keyword import.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationKeywordImportOutcome {
    /// Newly inserted keywords.
    pub inserted: usize,
    /// Keywords skipped because they already existed.
    pub duplicates: usize,
}

/// Admin moderation keyword list filters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminModerationKeywordPageQuery {
    /// Case-insensitive search over keyword, note, source, and category codes.
    #[serde(default)]
    pub search: Option<String>,
}

/// Page of configured moderation keywords.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationKeywordsPage {
    /// Page rows.
    pub keywords: Vec<ModerationKeyword>,
    /// Total rows matching the query before pagination.
    pub total: usize,
    /// Page limit.
    pub limit: usize,
    /// Page offset.
    pub offset: usize,
    /// Whether another page is available.
    pub has_more: bool,
}

/// Apply the admin keyword search and pagination contract to an in-memory list.
pub fn page_moderation_keywords(
    keywords: Vec<ModerationKeyword>,
    query: &AdminModerationKeywordPageQuery,
    page: AdminPageRequest,
) -> ModerationKeywordsPage {
    let search = normalized_keyword_search(query.search.as_deref());
    let compact_search = search
        .as_deref()
        .map(compact_keyword_search_text)
        .filter(|value| !value.is_empty());
    let filtered: Vec<ModerationKeyword> = keywords
        .into_iter()
        .filter(|keyword| {
            moderation_keyword_matches_query(keyword, search.as_deref(), compact_search.as_deref())
        })
        .collect();
    let total = filtered.len();
    let limit = page.limit.max(1);
    let keywords = filtered
        .into_iter()
        .skip(page.offset)
        .take(limit)
        .collect::<Vec<_>>();
    ModerationKeywordsPage {
        has_more: page.has_more(keywords.len(), total),
        keywords,
        total,
        limit,
        offset: page.offset,
    }
}

fn normalized_keyword_search(search: Option<&str>) -> Option<String> {
    search
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase)
}

fn compact_keyword_search_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(|character| character.to_lowercase())
        .collect()
}

fn moderation_keyword_matches_query(
    keyword: &ModerationKeyword,
    search: Option<&str>,
    compact_search: Option<&str>,
) -> bool {
    let Some(search) = search else {
        return true;
    };
    let keyword_text = keyword.keyword.to_lowercase();
    if keyword_text.contains(search) {
        return true;
    }
    if compact_search
        .is_some_and(|needle| compact_keyword_search_text(&keyword.keyword).contains(needle))
    {
        return true;
    }
    if keyword
        .note
        .as_deref()
        .is_some_and(|note| note.to_lowercase().contains(search))
    {
        return true;
    }
    if keyword.source.to_lowercase().contains(search) {
        return true;
    }
    keyword
        .categories
        .iter()
        .any(|category| category.to_lowercase().contains(search))
}

/// Banned-session card without the captured request payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationBannedSession {
    /// Stable row id.
    pub id: i64,
    /// Stable key for this exact hit inside this session.
    pub hit_key: String,
    /// Runtime ban key: `provider:key_id:session_id`.
    pub session_key: String,
    /// Provider family that produced the ban (`kiro` or `codex`).
    pub provider: String,
    /// Authenticated key id at ban time.
    pub key_id: String,
    /// Authenticated key display name at ban time.
    pub key_name: String,
    /// Client session id; empty when no stable session id was available.
    pub session_id: String,
    /// Normalized keyword that fired.
    pub matched_keyword: String,
    /// Risk-category codes of the keyword that fired.
    #[serde(default)]
    pub matched_categories: Vec<String>,
    /// Context snippet around the hit from the normalized request text.
    pub matched_context: String,
    /// Byte offset where the hit starts in normalized request text.
    pub match_start: i64,
    /// Byte offset just after the hit in normalized request text.
    pub match_end: i64,
    /// SHA-256 of normalized text before `match_start`, used to bind hit
    /// suppression to the exact reviewed preceding content.
    pub match_prefix_sha256: String,
    /// Hash of the keyword set active when this hit was recorded.
    pub keyword_set_hash: String,
    /// Gateway endpoint that received the blocked request.
    pub endpoint: String,
    /// Requested model, if known.
    pub model: String,
    /// Client IP at ban time.
    pub client_ip: String,
    /// `banned` or `unbanned`.
    pub status: String,
    /// Optional reviewer note recorded with the latest review action.
    pub review_note: Option<String>,
    /// Ban timestamp.
    pub banned_at_ms: i64,
    /// Latest review timestamp, if reviewed.
    pub reviewed_at_ms: Option<i64>,
    /// Update timestamp.
    pub updated_at_ms: i64,
}

/// Banned-session detail including the captured request payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationBannedSessionDetail {
    /// Session card.
    pub session: ModerationBannedSession,
    /// Captured request headers as a JSON object (sensitive values redacted).
    pub request_headers_json: String,
    /// Captured full request body JSON.
    pub request_body_json: String,
}

/// Insert payload for one new banned session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewModerationBannedSession {
    /// Stable key for this exact hit inside this session.
    pub hit_key: String,
    /// Runtime ban key: `provider:key_id:session_id`.
    pub session_key: String,
    /// Provider family (`kiro` or `codex`).
    pub provider: String,
    /// Authenticated key id.
    pub key_id: String,
    /// Authenticated key display name.
    pub key_name: String,
    /// Client session id; empty when derived from request content.
    pub session_id: String,
    /// Normalized keyword that fired.
    pub matched_keyword: String,
    /// Risk-category codes of the keyword that fired.
    pub matched_categories: Vec<String>,
    /// Context snippet around the hit.
    pub matched_context: String,
    /// Byte offset where the hit starts in normalized request text.
    pub match_start: i64,
    /// Byte offset just after the hit in normalized request text.
    pub match_end: i64,
    /// SHA-256 of normalized text before `match_start`.
    pub match_prefix_sha256: String,
    /// Hash of the keyword set active when this hit was recorded.
    pub keyword_set_hash: String,
    /// Gateway endpoint.
    pub endpoint: String,
    /// Requested model, if known.
    pub model: String,
    /// Client IP.
    pub client_ip: String,
    /// Captured request headers JSON object (sensitive values redacted).
    pub request_headers_json: String,
    /// Captured full request body JSON.
    pub request_body_json: String,
    /// Ban timestamp.
    pub banned_at_ms: i64,
}

/// One reviewed false-positive hit used by the runtime gate to suppress only
/// that exact repeat match.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationSuppressedHit {
    /// Runtime ban key: `provider:key_id:session_id`.
    pub session_key: String,
    /// Stable key for this exact hit inside this session.
    pub hit_key: String,
    /// Byte offset where the reviewed hit starts in normalized request text.
    pub match_start: i64,
    /// Byte offset just after the reviewed hit in normalized request text.
    pub match_end: i64,
    /// SHA-256 of normalized text before `match_start`.
    pub match_prefix_sha256: String,
    /// Hash of the keyword set active when this hit was recorded.
    pub keyword_set_hash: String,
}

/// One page of banned sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationBannedSessionsPage {
    /// Page rows.
    pub sessions: Vec<ModerationBannedSession>,
    /// Total rows matching the filter before pagination.
    pub total: usize,
    /// Page limit.
    pub limit: usize,
    /// Page offset.
    pub offset: usize,
    /// Whether another page is available.
    pub has_more: bool,
}

/// Admin banned-session list filters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminModerationBannedSessionPageQuery {
    /// `banned`, `unbanned`, or `all`/empty for every status.
    #[serde(default)]
    pub status: Option<String>,
    /// Case-insensitive search over hit/session/key/request metadata.
    #[serde(default)]
    pub search: Option<String>,
}

/// Active ban reference loaded into the runtime gate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationBannedSessionRef {
    /// Runtime ban key, kept internal because it contains the authenticated key
    /// id.
    pub session_key: String,
    /// Public-safe review id for the active hit.
    pub hit_key: String,
}

/// Compact startup/refresh snapshot for the in-memory moderation gate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationRuntimeSnapshot {
    /// All configured keywords.
    pub keywords: Vec<ModerationKeyword>,
    /// Session keys with `banned` status and their visible review ids.
    pub banned_sessions: Vec<ModerationBannedSessionRef>,
    /// Reviewed false-positive hits with `unbanned` status.
    pub suppressed_hits: Vec<ModerationSuppressedHit>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AdminPageRequest;

    fn keyword(
        id: i64,
        keyword: &str,
        categories: &[&str],
        note: Option<&str>,
    ) -> ModerationKeyword {
        ModerationKeyword {
            id,
            keyword: keyword.to_string(),
            categories: categories.iter().map(|value| value.to_string()).collect(),
            note: note.map(str::to_string),
            source: MODERATION_KEYWORD_SOURCE_TXT.to_string(),
            created_at_ms: id,
        }
    }

    #[test]
    fn moderation_keyword_query_matches_keyword_note_source_category_and_compacted_spacing() {
        let keywords = vec![
            keyword(1, "自 慰 描 写", &["sexual"], Some("manual review")),
            keyword(2, "build a bomb", &["weapons"], Some("danger note")),
            keyword(3, "carding tutorial", &["cyber"], None),
        ];

        let compacted = page_moderation_keywords(
            keywords.clone(),
            &AdminModerationKeywordPageQuery {
                search: Some("自慰".to_string()),
            },
            AdminPageRequest {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(compacted.total, 1);
        assert_eq!(compacted.keywords[0].keyword, "自 慰 描 写");

        let note = page_moderation_keywords(
            keywords.clone(),
            &AdminModerationKeywordPageQuery {
                search: Some("DANGER".to_string()),
            },
            AdminPageRequest {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(note.total, 1);
        assert_eq!(note.keywords[0].keyword, "build a bomb");

        let category = page_moderation_keywords(
            keywords,
            &AdminModerationKeywordPageQuery {
                search: Some("cyber".to_string()),
            },
            AdminPageRequest {
                limit: 10,
                offset: 0,
            },
        );
        assert_eq!(category.total, 1);
        assert_eq!(category.keywords[0].keyword, "carding tutorial");
    }

    #[test]
    fn moderation_keyword_page_reports_limit_offset_and_has_more() {
        let keywords = vec![
            keyword(4, "four", &[], None),
            keyword(3, "three", &[], None),
            keyword(2, "two", &[], None),
            keyword(1, "one", &[], None),
        ];

        let page = page_moderation_keywords(
            keywords,
            &AdminModerationKeywordPageQuery::default(),
            AdminPageRequest {
                limit: 2,
                offset: 1,
            },
        );

        assert_eq!(page.total, 4);
        assert_eq!(page.limit, 2);
        assert_eq!(page.offset, 1);
        assert!(page.has_more);
        assert_eq!(
            page.keywords
                .iter()
                .map(|keyword| keyword.keyword.as_str())
                .collect::<Vec<_>>(),
            vec!["three", "two"]
        );
    }
}
