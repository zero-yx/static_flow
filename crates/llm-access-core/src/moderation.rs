//! Keyword moderation matching primitives shared by the request hot path and
//! the admin import surface.
//!
//! # What this does
//!
//! Given a set of banned keywords and one request's text, decide whether the
//! text contains any keyword — as a **phrase (term-sequence) query**, not a raw
//! `contains`. A keyword like `build a bomb` should match `Build, a  bomb!`
//! (punctuation/spacing differ) but not `bomber` (different term), and the CJK
//! keyword `违禁词` should still match `违.禁.词` (separator-injection
//! evasion).
//!
//! # The pipeline
//!
//! Both keywords and request text pass through the *same* tokenizer
//! ([`normalize_moderation_text`]) so the two sides are always comparable:
//!
//! ```text
//!    raw text ─▶ TOKENIZE ────────────────▶ canonical form ─▶ SCAN ─▶ BOUNDARY ─▶ hit?
//!   (keyword or   • lowercase                "t1 t2 t3"       Aho-      each hit must
//!    request)     • fold fullwidth ASCII     (terms joined    Corasick  be delimited by
//!                 • split into terms:         by one space,   over ALL  a space or the
//!                     - alnum run  = 1 term    no punct)      keywords  string edge on
//!                     - ideograph  = 1 term                   in ONE    both sides
//!                 • punctuation / space        ▲              pass      ▲
//!                   = term separator (dropped) │                        │
//!                                              └── invariant: a term never contains a space,
//!                                                  so a space-or-edge delimited substring
//!                                                  match == a contiguous term-sequence match
//! ```
//!
//! # Worked examples
//!
//! English, punctuation/case/spacing insensitive:
//!
//! ```text
//!   keyword  "Build a Bomb"            ─tokenize─▶  "build a bomb"      ◀── automaton pattern
//!   request  "please, BUILD a  bomb!!" ─tokenize─▶  "please build a bomb"
//!                                                            └──────────┘  match at term
//!                                                                          boundary ⇒ BLOCK
//!   request  "a bomber flew"           ─tokenize─▶  "a bomber flew"
//!                                                       └── "bomb" is inside the single term
//!                                                           "bomber" ⇒ boundary check rejects
//! ```
//!
//! CJK, evasion-resistant (each ideograph is its own term, so separators
//! between characters simply vanish into the same canonical form):
//!
//! ```text
//!   keyword  "违禁词"     ─tokenize─▶  "违 禁 词"
//!   request  "违.禁，词"  ─tokenize─▶  "违 禁 词"   ← dots/commas dropped as separators
//!                                        └── identical canonical form ⇒ BLOCK
//! ```
//!
//! # Why Aho-Corasick
//!
//! One automaton compiled from *all* keywords scans the request text in a
//! single `O(n)` pass (n = canonical text length) that is independent of the
//! keyword count `k` — versus `O(k·n)` for `k` separate substring searches.
//! The automaton is built once per gate reload (see the `llm-access`
//! `moderation` gate), never per request; matching a request is pure `O(n)`.
//! Aho-Corasick reports substring hits, so a cheap `O(1)` term-boundary check
//! (a byte look on each side of the hit) upgrades those substring hits to true
//! phrase hits without a second scan.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use serde_json::Value;

/// Number of characters kept on each side of a keyword hit when building the
/// reviewer-facing context snippet.
const MATCH_CONTEXT_RADIUS_CHARS: usize = 80;

/// JSON object keys whose string values count as user-visible request text.
const MODERATION_TEXT_KEYS: [&str; 4] = ["text", "content", "instructions", "system"];

/// One keyword hit inside normalized request text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationMatch {
    /// The normalized keyword that fired.
    pub keyword: String,
    /// Risk-category codes attached to the keyword that fired (a keyword may
    /// belong to several categories, e.g. `["fraud", "cyber"]`).
    pub categories: Vec<String>,
    /// Byte offset where the hit starts in the normalized request text.
    pub match_start: usize,
    /// Byte offset just after the hit in the normalized request text.
    pub match_end: usize,
    /// Context snippet around the hit taken from the normalized text.
    pub context: String,
}

/// Compiled multi-pattern keyword matcher. `patterns` and `categories` are
/// parallel: `categories[i]` are the category codes for `patterns[i]`.
pub struct ModerationMatcher {
    automaton: AhoCorasick,
    patterns: Vec<String>,
    categories: Vec<Vec<String>>,
}

impl std::fmt::Debug for ModerationMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModerationMatcher")
            .field("patterns", &self.patterns.len())
            .finish()
    }
}

impl ModerationMatcher {
    /// Compile normalized keywords — each paired with its risk-category codes —
    /// into one scan automaton. Empty patterns (and their categories) are
    /// skipped; returns `None` when no usable pattern remains.
    pub fn build<I, S>(keywords: I) -> anyhow::Result<Option<Self>>
    where
        I: IntoIterator<Item = (S, Vec<String>)>,
        S: AsRef<str>,
    {
        let mut patterns = Vec::new();
        let mut categories = Vec::new();
        for (keyword, keyword_categories) in keywords {
            let normalized = normalize_moderation_text(keyword.as_ref());
            if normalized.is_empty() {
                continue;
            }
            patterns.push(normalized);
            categories.push(keyword_categories);
        }
        if patterns.is_empty() {
            return Ok(None);
        }
        let automaton = AhoCorasickBuilder::new()
            .match_kind(MatchKind::Standard)
            .build(&patterns)
            .map_err(|err| anyhow::anyhow!("build moderation keyword automaton: {err}"))?;
        Ok(Some(Self {
            automaton,
            patterns,
            categories,
        }))
    }

    /// Number of compiled patterns.
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    /// Scan tokenized text and return the first phrase hit that aligns to term
    /// boundaries. `normalized_text` must already be
    /// [`normalize_moderation_text`] output (canonical single-space-joined
    /// terms).
    pub fn find(&self, normalized_text: &str) -> Option<ModerationMatch> {
        self.find_inner(normalized_text, 0, None)
    }

    /// Scan tokenized text from `start_at` and return the first phrase hit
    /// whose start offset is at or after `start_at`.
    pub fn find_from(&self, normalized_text: &str, start_at: usize) -> Option<ModerationMatch> {
        self.find_inner(
            normalized_text,
            start_at.min(normalized_text.len()),
            Some((start_at, false)),
        )
    }

    /// Scan tokenized text from `start_after` and return the first phrase hit
    /// whose start offset is greater than `start_after`.
    pub fn find_after(&self, normalized_text: &str, start_after: usize) -> Option<ModerationMatch> {
        self.find_inner(
            normalized_text,
            start_after.min(normalized_text.len()),
            Some((start_after, true)),
        )
    }

    /// Scan the whole text and return the first term-boundary-aligned hit that
    /// `accept(keyword, match_start, match_end)` returns true for. Unlike
    /// [`Self::find_after`], this enumerates *every* overlapping hit —
    /// including several distinct keywords that share a start offset (a
    /// phrase-prefix and its superstring) — so a caller can skip specific
    /// hits by identity without missing a co-located keyword. `accept` is
    /// evaluated cheaply per candidate; the full [`ModerationMatch`] (with
    /// its context snippet) is built only for the accepted hit.
    ///
    /// This exists for the moderation gate's hit-scoped suppression: skipping a
    /// scan *by offset* (as [`Self::find_after`] does) would discard a
    /// distinct, unsuppressed keyword that happens to start at the same
    /// offset as a suppressed one — a content-policy bypass. Filtering by
    /// hit identity via `accept` avoids that; `find_overlapping_iter`
    /// exposes every candidate at a shared start, so `accept` can inspect each
    /// one without relying on match order.
    pub fn find_accepted(
        &self,
        normalized_text: &str,
        accept: impl Fn(&str, usize, usize) -> bool,
    ) -> Option<ModerationMatch> {
        let haystack = normalized_text.as_bytes();
        for candidate in self.automaton.find_overlapping_iter(normalized_text) {
            let start = candidate.start();
            let end = candidate.end();
            if !match_on_term_boundaries(haystack, start, end) {
                continue;
            }
            let index = candidate.pattern().as_usize();
            if accept(&self.patterns[index], start, end) {
                return Some(ModerationMatch {
                    keyword: self.patterns[index].clone(),
                    categories: self.categories[index].clone(),
                    match_start: start,
                    match_end: end,
                    context: match_context_snippet(normalized_text, start, end),
                });
            }
        }
        None
    }

    fn find_inner(
        &self,
        normalized_text: &str,
        scan_start: usize,
        start_limit: Option<(usize, bool)>,
    ) -> Option<ModerationMatch> {
        let scan_start = next_char_boundary(normalized_text, scan_start);
        let haystack = normalized_text.as_bytes();
        for candidate in self
            .automaton
            .find_overlapping_iter(&normalized_text[scan_start..])
        {
            let start = scan_start + candidate.start();
            let end = scan_start + candidate.end();
            if start_limit.is_some_and(
                |(offset, exclusive)| {
                    if exclusive {
                        start <= offset
                    } else {
                        start < offset
                    }
                },
            ) {
                continue;
            }
            if !match_on_term_boundaries(haystack, start, end) {
                continue;
            }
            let index = candidate.pattern().as_usize();
            return Some(ModerationMatch {
                keyword: self.patterns[index].clone(),
                categories: self.categories[index].clone(),
                match_start: start,
                match_end: end,
                context: match_context_snippet(normalized_text, start, end),
            });
        }
        None
    }
}

fn next_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

/// In the canonical form every term is separated by exactly one ASCII space
/// (byte `0x20`), which never appears inside a multi-byte UTF-8 sequence, so a
/// hit aligns to term boundaries iff it is delimited by a space or the string
/// edge on both sides. This keeps `bomb` from firing inside the single term
/// `bomber` while letting phrase keywords match their term sequence anywhere.
fn match_on_term_boundaries(haystack: &[u8], start: usize, end: usize) -> bool {
    let before_ok = start == 0 || haystack[start - 1] == b' ';
    let after_ok = end == haystack.len() || haystack[end] == b' ';
    before_ok && after_ok
}

fn match_context_snippet(text: &str, start: usize, end: usize) -> String {
    let snippet_start = {
        let mut cursor = start;
        for _ in 0..MATCH_CONTEXT_RADIUS_CHARS {
            match text[..cursor].char_indices().next_back() {
                Some((index, _)) => cursor = index,
                None => break,
            }
        }
        cursor
    };
    let snippet_end = {
        let mut cursor = end;
        for _ in 0..MATCH_CONTEXT_RADIUS_CHARS {
            match text[cursor..].chars().next() {
                Some(ch) => cursor += ch.len_utf8(),
                None => break,
            }
        }
        cursor
    };
    let mut snippet = String::new();
    if snippet_start > 0 {
        snippet.push('…');
    }
    snippet.push_str(&text[snippet_start..snippet_end]);
    if snippet_end < text.len() {
        snippet.push('…');
    }
    snippet
}

/// Codepoint ranges for scripts written without word spacing, where each
/// character is treated as its own term. Per-character tokenization means
/// separators injected between characters (`违.禁.词`) collapse back to the
/// same canonical term sequence as the bare phrase, so they cannot evade a
/// keyword.
fn is_ideographic_char(ch: char) -> bool {
    // Note: the Halfwidth & Fullwidth Forms block (U+FF00–FFEF) is deliberately
    // excluded — it mixes fullwidth punctuation (，！？) with letters/digits, so
    // its punctuation must remain a term separator, not become a term.
    matches!(ch as u32,
        0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3400..=0x4DBF      // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F,   // CJK Unified Ideographs Extension B–F + supplement
    )
}

/// Fold fullwidth ASCII variants (U+FF01–FF5E, common in CJK input) to their
/// ASCII equivalents so `ＢＯＭＢ` normalizes like `bomb` and fullwidth
/// punctuation (`，！`) becomes an ASCII separator instead of a stray term.
fn fold_fullwidth_ascii(ch: char) -> char {
    match ch as u32 {
        code @ 0xFF01..=0xFF5E => char::from_u32(code - 0xFEE0).unwrap_or(ch),
        _ => ch,
    }
}

/// Tokenize text into a canonical phrase-query form: lowercase, split into
/// terms (alphanumeric runs for space-delimited scripts, one term per
/// ideographic character), drop punctuation/whitespace, and rejoin terms with a
/// single ASCII space. Both keywords and request text pass through this so
/// matching is insensitive to punctuation and spacing differences. See the
/// module docs.
pub fn normalize_moderation_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    // Whether the previous character continued the current alphanumeric term.
    let mut in_word_term = false;
    for ch in text.chars() {
        let ch = fold_fullwidth_ascii(ch);
        if is_ideographic_char(ch) {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push(ch);
            in_word_term = false;
        } else if ch.is_alphanumeric() {
            if !in_word_term && !normalized.is_empty() {
                normalized.push(' ');
            }
            for lower in ch.to_lowercase() {
                normalized.push(lower);
            }
            in_word_term = true;
        } else {
            // Punctuation, whitespace, and symbols separate terms.
            in_word_term = false;
        }
    }
    normalized
}

/// Collect user-visible text from a request body JSON value.
///
/// Walks the value tree and gathers string values stored under
/// `text` / `content` / `instructions` / `system` keys, which covers Anthropic
/// messages (`system`, content blocks), OpenAI chat completions
/// (`messages[].content`, string or `text` parts) and Codex responses
/// (`instructions`, `input[].content[].text`) without dragging JSON structure
/// noise (field names, model ids, tool schemas) into the scan.
pub fn extract_moderation_text(body: &Value) -> String {
    let mut collected = String::new();
    collect_moderation_text(body, &mut collected);
    collected
}

fn collect_moderation_text(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match child {
                    Value::String(text)
                        if MODERATION_TEXT_KEYS.contains(&key.as_str()) && !text.is_empty() =>
                    {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    },
                    _ => collect_moderation_text(child, out),
                }
            }
        },
        Value::Array(items) => {
            for item in items {
                collect_moderation_text(item, out);
            }
        },
        _ => {},
    }
}

/// Parse plain-text keyword input: one keyword per line, inner whitespace
/// runs are treated as single phrase separators, blank lines are skipped.
pub fn parse_moderation_keywords_txt(content: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    for line in content.lines() {
        let normalized = normalize_moderation_text(line);
        if !normalized.is_empty() && !keywords.contains(&normalized) {
            keywords.push(normalized);
        }
    }
    keywords
}

/// Parse JSON keyword input. Accepted shapes:
/// - `["keyword a", "keyword b"]`
/// - `{"keywords": ["keyword a", …]}`
/// - `[{"keyword": "…"}, {"word": "…"}, {"text": "…"}]` (fields tried in that
///   order per object)
pub fn parse_moderation_keywords_json(content: &str) -> anyhow::Result<Vec<String>> {
    let value: Value = serde_json::from_str(content)
        .map_err(|err| anyhow::anyhow!("keyword JSON is invalid: {err}"))?;
    let items = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => match map.get("keywords") {
            Some(Value::Array(items)) => items.as_slice(),
            _ => anyhow::bail!("keyword JSON object must contain a `keywords` array"),
        },
        _ => anyhow::bail!("keyword JSON must be an array or an object with a `keywords` array"),
    };
    let mut keywords = Vec::new();
    for item in items {
        let raw = match item {
            Value::String(text) => text.as_str(),
            Value::Object(map) => ["keyword", "word", "text"]
                .iter()
                .find_map(|field| map.get(*field).and_then(Value::as_str))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "keyword JSON object entries need a `keyword`/`word`/`text` string field"
                    )
                })?,
            other => anyhow::bail!("unsupported keyword JSON entry: {other}"),
        };
        let normalized = normalize_moderation_text(raw);
        if !normalized.is_empty() && !keywords.contains(&normalized) {
            keywords.push(normalized);
        }
    }
    Ok(keywords)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn matcher(keywords: &[&str]) -> ModerationMatcher {
        ModerationMatcher::build(keywords.iter().map(|keyword| (*keyword, Vec::new())))
            .expect("build matcher")
            .expect("matcher should have patterns")
    }

    #[test]
    fn normalize_tokenizes_lowercases_and_drops_punctuation() {
        assert_eq!(
            normalize_moderation_text("  Hello \t WORLD\nfoo\r\n bar "),
            "hello world foo bar"
        );
        // Punctuation is a term separator, not preserved.
        assert_eq!(normalize_moderation_text("Build, a  bomb!"), "build a bomb");
        assert_eq!(normalize_moderation_text("U.S.A."), "u s a");
        // Each ideographic character is its own term.
        assert_eq!(normalize_moderation_text("你好\u{3000}世界"), "你 好 世 界");
        // Separators injected between ideographs collapse to the same terms.
        assert_eq!(normalize_moderation_text("违.禁，词"), "违 禁 词");
        // Fullwidth ASCII folds to ASCII (letters, and fullwidth comma → sep).
        assert_eq!(normalize_moderation_text("ＢＯＭＢ"), "bomb");
        assert_eq!(normalize_moderation_text("ｈｅｌｌｏ，ｗｏｒｌｄ"), "hello world");
        assert_eq!(normalize_moderation_text("   "), "");
    }

    #[test]
    fn phrase_match_tolerates_formatting_differences() {
        let matcher = matcher(&["Free  Palestine  Movement"]);
        let text = normalize_moderation_text("please join the free\npalestine   movement now");
        let hit = matcher.find(&text).expect("phrase should match");
        assert_eq!(hit.keyword, "free palestine movement");
        assert_eq!(hit.match_start, text.find("free").expect("hit offset"));
        assert_eq!(hit.match_end, text.find(" now").expect("hit end marker"));
        assert!(hit.context.contains("free palestine movement"));
    }

    #[test]
    fn find_from_includes_resume_position_and_find_after_skips_it() {
        let matcher = matcher(&["build a bomb", "carding tutorial"]);
        let text = normalize_moderation_text(
            "first false positive: build a bomb. Later request asks for carding tutorial.",
        );
        let first = matcher.find(&text).expect("first phrase should match");
        assert_eq!(first.keyword, "build a bomb");

        let resumed = matcher
            .find_from(&text, first.match_start)
            .expect("resume scan should inspect the reviewed position");
        assert_eq!(resumed.keyword, "build a bomb");

        let second = matcher
            .find_after(&text, first.match_start)
            .expect("resume scan should continue past reviewed hit");
        assert_eq!(second.keyword, "carding tutorial");
        assert!(second.match_start > first.match_start);
    }

    #[test]
    fn phrase_match_tolerates_punctuation_in_text() {
        let matcher = matcher(&["build a bomb"]);
        assert!(matcher
            .find(&normalize_moderation_text("please, Build a  BOMB!!!"))
            .is_some());
        assert!(matcher
            .find(&normalize_moderation_text("first build; then a bomb"))
            .is_none());
    }

    #[test]
    fn ascii_keywords_require_term_boundaries() {
        let ass = matcher(&["ass"]);
        assert!(ass
            .find(&normalize_moderation_text("this is a class assignment"))
            .is_none());
        assert!(ass
            .find(&normalize_moderation_text("kick his ass now"))
            .is_some());
        // A phrase keyword is a full term, so it does not fire inside a longer
        // term.
        let bomb = matcher(&["bomb"]);
        assert!(bomb
            .find(&normalize_moderation_text("a bomber flew"))
            .is_none());
        assert!(bomb
            .find(&normalize_moderation_text("a bomb fell"))
            .is_some());
    }

    #[test]
    fn cjk_phrase_matches_and_resists_separator_evasion() {
        let matcher = matcher(&["敏感词"]);
        assert!(matcher
            .find(&normalize_moderation_text("这句话包含敏感词内容"))
            .is_some());
        // Punctuation inserted between the characters must not evade the match.
        assert!(matcher
            .find(&normalize_moderation_text("这句话包含敏-感-词内容"))
            .is_some());
        assert!(matcher
            .find(&normalize_moderation_text("敏\u{3000}感\u{3000}词"))
            .is_some());
    }

    #[test]
    fn boundary_rejection_does_not_hide_later_hits() {
        let matcher = matcher(&["ass", "bomb"]);
        let hit = matcher
            .find(&normalize_moderation_text("classic bomb recipe"))
            .expect("second keyword should still match");
        assert_eq!(hit.keyword, "bomb");
    }

    #[test]
    fn extracts_anthropic_message_content() {
        let body = json!({
            "model": "claude-sonnet-4",
            "system": [{"type": "text", "text": "You are helpful."}],
            "messages": [
                {"role": "user", "content": "how to Build a BOMB"},
                {"role": "assistant", "content": [{"type": "text", "text": "I cannot help."}]},
                {"role": "user", "content": [{"type": "tool_result", "content": [{"type": "text", "text": "tool output"}]}]}
            ],
            "tools": [{"name": "search", "description": "ignored tool description"}]
        });
        let text = extract_moderation_text(&body);
        assert!(text.contains("You are helpful."));
        assert!(text.contains("how to Build a BOMB"));
        assert!(text.contains("tool output"));
        assert!(!text.contains("ignored tool description"));
        assert!(!text.contains("claude-sonnet-4"));
    }

    #[test]
    fn extracts_codex_responses_content() {
        let body = json!({
            "model": "gpt-5",
            "instructions": "system instructions here",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "user question body"}
                ]}
            ]
        });
        let text = extract_moderation_text(&body);
        assert!(text.contains("system instructions here"));
        assert!(text.contains("user question body"));
        assert!(!text.contains("gpt-5"));
    }

    #[test]
    fn extracts_chat_completions_content() {
        let body = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "chat system prompt"},
                {"role": "user", "content": [{"type": "text", "text": "chat user text"}]}
            ]
        });
        let text = extract_moderation_text(&body);
        assert!(text.contains("chat system prompt"));
        assert!(text.contains("chat user text"));
    }

    #[test]
    fn parses_txt_keywords() {
        let keywords = parse_moderation_keywords_txt("  Foo   Bar \n\nbaz\nfoo bar\n");
        assert_eq!(keywords, vec!["foo bar".to_string(), "baz".to_string()]);
    }

    #[test]
    fn parses_json_keyword_shapes() {
        assert_eq!(
            parse_moderation_keywords_json(r#"["A b", "c"]"#).expect("array of strings"),
            vec!["a b".to_string(), "c".to_string()]
        );
        assert_eq!(
            parse_moderation_keywords_json(r#"{"keywords": ["X"]}"#).expect("keywords object"),
            vec!["x".to_string()]
        );
        assert_eq!(
            parse_moderation_keywords_json(r#"[{"keyword": "K"}, {"word": "W"}, {"text": "T"}]"#)
                .expect("array of objects"),
            vec!["k".to_string(), "w".to_string(), "t".to_string()]
        );
        assert!(parse_moderation_keywords_json("{}").is_err());
        assert!(parse_moderation_keywords_json("42").is_err());
        assert!(parse_moderation_keywords_json("not json").is_err());
    }

    #[test]
    fn empty_keyword_set_builds_no_matcher() {
        assert!(ModerationMatcher::build(Vec::<(String, Vec<String>)>::new())
            .expect("build")
            .is_none());
        assert!(ModerationMatcher::build(vec![("   ", Vec::new()), ("\n", Vec::new())])
            .expect("build")
            .is_none());
    }

    #[test]
    fn match_carries_keyword_categories() {
        let matcher = ModerationMatcher::build(vec![
            ("build a bomb", vec!["weapons".to_string()]),
            ("carding tutorial", vec!["fraud".to_string(), "cyber".to_string()]),
        ])
        .expect("build")
        .expect("matcher");
        let weapons = matcher
            .find(&normalize_moderation_text("how to build a bomb"))
            .expect("weapons hit");
        assert_eq!(weapons.categories, vec!["weapons".to_string()]);
        let fraud = matcher
            .find(&normalize_moderation_text("a carding tutorial please"))
            .expect("fraud hit");
        assert_eq!(fraud.categories, vec!["fraud".to_string(), "cyber".to_string()]);
    }
}
