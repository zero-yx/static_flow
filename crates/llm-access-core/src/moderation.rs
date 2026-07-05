//! Keyword moderation matching primitives shared by the request hot path and
//! the admin import surface.
//!
//! # What this does
//!
//! Given a set of banned keywords and one request's text, decide whether the
//! text contains any keyword — as a **phrase (term-sequence) query**, not a raw
//! `contains`. A keyword like `build a bomb` should match `Build, a  bomb!`
//! (punctuation/spacing differ) but not `bomber` (different term), and the
//! Chinese keyword `口交` should match `口，交` while not firing inside
//! `接口交互`.
//!
//! # The pipeline
//!
//! Keyword text and request text pass through the *same* tokenizer so the two
//! sides are always comparable. Keywords in isolation use the base analyzer
//! ([`normalize_moderation_text`]); request text uses the matcher's
//! keyword-seeded analyzer ([`ModerationMatcher::normalize`]) — see
//! "Keyword-aware segmentation" below.
//!
//! ```text
//!    raw text ─▶ CHAR FILTER ─▶ TOKENIZE ─────────────▶ canonical form ─▶ SCAN ─▶ BOUNDARY ─▶ hit?
//!   (keyword or   • NFKC          • Han spans: Jieba      "t1 t2 t3"       Aho-      each hit must
//!    request)     • drop           • other spans: ICU     (terms joined    Corasick  be delimited by
//!                   invisible        word segmenter       by one space,   over ALL  a space or the
//!                   format/         • lowercase terms      no punct)      keywords  string edge on
//!                   combining                                              in ONE    both sides
//!                 • Han-internal      ▲                                    pass      ▲
//!                   separators        └── invariant: a term never contains a space,  │
//!                   drop; other           so a space-or-edge delimited substring ────┘
//!                   separators ⇒          match == a contiguous term-sequence match
//!                   one space
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
//! Chinese, evasion-resistant without subword false positives:
//!
//! ```text
//!   keyword  "口交"        ─tokenize─▶  "口交"        (seeded as an atomic dict word)
//!   request  "口，交"      ─tokenize─▶  "口交"        ← Han-internal punctuation dropped ⇒ BLOCK
//!   request  "请问口交好吗" ─tokenize─▶ "请问 口交 好 吗" ← seeding keeps 口交 whole ⇒ BLOCK
//!   request  "接口交互"    ─tokenize─▶  "接口 交互"   ← 接口·交互 outweigh the seed ⇒ ALLOW
//! ```
//!
//! # Keyword-aware segmentation
//!
//! Word segmentation is context-sensitive, so a keyword tokenized alone is not
//! guaranteed to reappear as a token when embedded in text — Jieba merges `口交`
//! into the common word `交好` in `口交好`, letting it evade. Each matcher
//! therefore seeds its keywords into a private Jieba dictionary
//! ([`ModerationMatcher::build`]) and normalizes request text through that same
//! seeded analyzer, biasing segmentation to keep keywords whole wherever their
//! characters are adjacent. The seed strength ([`KEYWORD_SEED_FREQ`]) beats
//! context merges while still losing to genuine multi-character words like
//! `接口`/`交互`; the residual failure direction is over-blocking (fail-closed).
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

use std::collections::HashSet;
use std::sync::OnceLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use icu_segmenter::{options::WordBreakInvariantOptions, WordSegmenter, WordSegmenterBorrowed};
use jieba_rs::Jieba;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

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
/// parallel: `categories[i]` are the category codes for `patterns[i]`. The
/// `analyzer` is seeded with this set's keywords and is the *only* correct way
/// to normalize request text for this matcher (see [`Self::normalize`]).
pub struct ModerationMatcher {
    automaton: AhoCorasick,
    patterns: Vec<String>,
    categories: Vec<Vec<String>>,
    analyzer: ModerationAnalyzer,
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
        // Collect first: the keyword strings are needed twice — once to seed the
        // analyzer's dictionary, then again to normalize each into an AC pattern
        // through that same seeded analyzer.
        let keywords: Vec<(S, Vec<String>)> = keywords.into_iter().collect();
        // No keywords at all: skip cloning the dictionary the seeded analyzer
        // would build only to discard.
        if keywords.is_empty() {
            return Ok(None);
        }
        let analyzer = ModerationAnalyzer::seeded(keywords.iter().map(|(kw, _)| kw.as_ref()));

        let mut patterns = Vec::new();
        let mut categories = Vec::new();
        for (keyword, keyword_categories) in keywords {
            let normalized = analyzer.normalize(keyword.as_ref());
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
            analyzer,
        }))
    }

    /// Normalize request text through this matcher's keyword-seeded analyzer.
    /// Request text MUST be normalized here rather than through the free
    /// [`normalize_moderation_text`], so keyword spans survive as tokens in the
    /// surrounding Chinese exactly as the compiled patterns do. Feed the result
    /// to [`Self::find`] and friends.
    pub fn normalize(&self, text: &str) -> String {
        self.analyzer.normalize(text)
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

/// Seed frequency injected for each moderation keyword's Han span into the
/// per-matcher [`Jieba`] dictionary.
///
/// Word segmentation is context-sensitive: on its own `口交` is a token, but in
/// `请问口交好吗` Jieba prefers the common word `交好` and splits the keyword,
/// and in `口交口交口交` it re-segments across the repeats — both let the
/// keyword slip through. Injecting the keyword as a dictionary word biases the
/// max-probability segmentation to keep it whole wherever its characters are
/// adjacent, which closes that evasion.
///
/// The value trades recall against precision on a log-frequency contest against
/// the surrounding words (default dict total ≈ 6.0e7):
/// - Recall floor: beating a merge such as `口|交好` (freq 20778 · 43) needs a
///   seed of only a few dozen, so any large value blocks the evasions.
/// - Precision ceiling: to *not* steal the boundary in `接口交互` (`接口`·`交互`
///   = 814 · 311, splittable only by paying the singletons `接`·`互` =
///   10515 · 2051) the seed must stay below ≈ 7.0e5.
///
/// 60_000 sits an order of magnitude inside both bounds, biased toward recall;
/// the residual failure direction is over-blocking (fail-closed), the safe bias
/// for a moderation gate. This is also the empirical knee: on a 187-case
/// adversarial 口交 corpus it gave the fewest false positives (4) while recall
/// was already saturated — raising it to 150k/300k/500k left the two residual
/// misses (口交通/口交易所, where the strong words 交通/交易所 dominate) in place
/// but grew false positives to 9/11/17.
const KEYWORD_SEED_FREQ: usize = 60_000;

/// Shared text analyzer: NFKC folding + noise stripping, then word segmentation
/// (Jieba for Han spans, the ICU word segmenter for everything else). Instances
/// are immutable after construction and safe to share across threads.
struct ModerationAnalyzer {
    jieba: Jieba,
    word_segmenter: WordSegmenterBorrowed<'static>,
}

/// Which segmenter owns a maximal same-script run. Han runs go to Jieba (which
/// models Chinese word boundaries); every other script goes to the ICU word
/// segmenter (Latin, Cyrillic, kana, Thai, digits, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenRunKind {
    Han,
    Other,
}

impl ModerationAnalyzer {
    /// The process-wide base analyzer: the default Jieba dictionary with no
    /// keywords seeded. Used to normalize keywords in isolation — import,
    /// dedup, set hashing — where there is no request context to protect.
    fn base() -> &'static ModerationAnalyzer {
        static BASE: OnceLock<ModerationAnalyzer> = OnceLock::new();
        BASE.get_or_init(|| ModerationAnalyzer::from_jieba(Jieba::new()))
    }

    fn from_jieba(jieba: Jieba) -> Self {
        Self {
            jieba,
            word_segmenter: WordSegmenter::new_auto(WordBreakInvariantOptions::default()),
        }
    }

    /// A keyword-aware analyzer: the base dictionary plus every keyword's Han
    /// span injected as an atomic word (see [`KEYWORD_SEED_FREQ`]). Request
    /// text must be normalized through the *same* seeded analyzer that compiled
    /// the keyword patterns, so a keyword survives as a token in the request's
    /// surrounding Chinese exactly as it does on its own.
    fn seeded<'a>(keywords: impl IntoIterator<Item = &'a str>) -> Self {
        let mut jieba = Self::base().jieba.clone();
        for keyword in keywords {
            let filtered = filter_moderation_chars(keyword);
            for_each_han_run(&filtered, |run| {
                jieba.add_word(run, Some(KEYWORD_SEED_FREQ), None);
            });
        }
        Self::from_jieba(jieba)
    }

    /// Normalize text to canonical phrase-query form: a single ASCII space
    /// joining lowercased, NFKC-folded terms. A term never contains a space, so
    /// a later space-or-edge delimited substring match is exactly a contiguous
    /// term-sequence match. See the module docs.
    fn normalize(&self, text: &str) -> String {
        let filtered = filter_moderation_chars(text);
        let mut out = String::with_capacity(filtered.len());
        // Accumulate a maximal same-script run, flushing it whenever the script
        // changes or a separating space is reached, so each run reaches the
        // segmenter that understands it.
        let mut run: Option<(usize, TokenRunKind)> = None;
        for (index, ch) in filtered.char_indices() {
            if ch == ' ' {
                if let Some((start, kind)) = run.take() {
                    self.push_run_terms(&filtered[start..index], kind, &mut out);
                }
                continue;
            }
            let kind = if is_han_char(ch) { TokenRunKind::Han } else { TokenRunKind::Other };
            match run {
                Some((start, current_kind)) if current_kind != kind => {
                    self.push_run_terms(&filtered[start..index], current_kind, &mut out);
                    run = Some((index, kind));
                },
                Some(_) => {},
                None => run = Some((index, kind)),
            }
        }
        if let Some((start, kind)) = run {
            self.push_run_terms(&filtered[start..], kind, &mut out);
        }
        out
    }

    /// Segment one same-script run and append its word-like terms to `out`.
    fn push_run_terms(&self, run: &str, kind: TokenRunKind, out: &mut String) {
        match kind {
            TokenRunKind::Han => {
                for token in self.jieba.cut(run, true) {
                    push_canonical_term(token.word, out);
                }
            },
            TokenRunKind::Other => self.push_icu_terms(run, out),
        }
    }

    /// Append the word-like segments of a non-Han run per ICU word boundaries.
    /// Non-word segments are skipped (the run is already reduced to
    /// alphanumerics, so there should be none).
    fn push_icu_terms(&self, run: &str, out: &mut String) {
        let mut boundaries = self.word_segmenter.segment_str(run).iter_with_word_type();
        let Some((mut start, _)) = boundaries.next() else {
            return;
        };
        for (end, word_type) in boundaries {
            if word_type.is_word_like() {
                push_canonical_term(&run[start..end], out);
            }
            start = end;
        }
    }
}

/// Tokenize text into the canonical phrase-query form via the process-wide base
/// analyzer (no moderation keywords seeded). Use this for keyword text in
/// isolation — import parsing, dedup, and set hashing. Request text is instead
/// normalized through [`ModerationMatcher::normalize`], whose analyzer is seeded
/// with the active keyword set. See the module docs.
pub fn normalize_moderation_text(text: &str) -> String {
    ModerationAnalyzer::base().normalize(text)
}

/// Append `term` to `out` as one lowercased term, prefixed by a single ASCII
/// space when `out` already holds a term. `term` is always a slice of
/// [`filter_moderation_chars`] output, so it carries no format/combining noise
/// and no separators — only case folding remains to apply.
fn push_canonical_term(term: &str, out: &mut String) {
    if term.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    for ch in term.chars() {
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
}

/// Char-filter pass shared by every analyzer: NFKC-fold, drop invisible
/// format/combining characters, and reduce the text to alphanumeric and Han
/// characters separated by single ASCII spaces. Separators collapse to one
/// space, except a separator run sitting *between two Han characters* is dropped
/// entirely — that is what lets `口，交` tokenize identically to `口交`
/// (separator-injection evasion) without gluing across a real word boundary.
///
/// Single pass, `O(n)`: the drop-or-space decision for a separator run is
/// deferred until the next kept character is known, so no character is ever
/// re-scanned. (An earlier version scanned outward from each separator for the
/// nearest non-noise neighbours, which is quadratic on long separator runs.)
fn filter_moderation_chars(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Was the most recently kept character a Han character?
    let mut prev_kept_han = false;
    // Have we seen ≥1 separator since the last kept character?
    let mut pending_separator = false;
    for ch in text.nfkc() {
        if is_ignored_format_char(ch) || is_combining_mark(ch) {
            // Invisible: neither a term character nor a real separator. Drop it
            // without breaking a pending Han-internal run (`口\u{200b}，交`).
            continue;
        }
        if is_token_separator_char(ch) {
            pending_separator = true;
            continue;
        }
        let is_han = is_han_char(ch);
        if pending_separator {
            // Drop the deferred run when it is leading (nothing kept yet) or
            // sits strictly between two Han characters; otherwise it delimits
            // terms and collapses to a single space.
            let drop_separator = out.is_empty() || (prev_kept_han && is_han);
            if !drop_separator {
                out.push(' ');
            }
            pending_separator = false;
        }
        out.push(ch);
        prev_kept_han = is_han;
    }
    out
}

/// Invoke `f` on each maximal run of Han characters in already-filtered text.
/// Used to seed keyword spans into a Jieba dictionary as atomic words.
fn for_each_han_run(filtered: &str, mut f: impl FnMut(&str)) {
    let mut start: Option<usize> = None;
    for (index, ch) in filtered.char_indices() {
        if is_han_char(ch) {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            f(&filtered[begin..index]);
        }
    }
    if let Some(begin) = start {
        f(&filtered[begin..]);
    }
}

/// A character that separates terms: whitespace, control characters, and any
/// punctuation or symbol. Alphanumerics and Han characters are *not* separators
/// (the `!is_han_char` clause is redundant with `is_alphanumeric` for Han but
/// kept explicit so the intent survives if the Han range is retuned).
fn is_token_separator_char(ch: char) -> bool {
    ch.is_whitespace() || ch.is_control() || (!ch.is_alphanumeric() && !is_han_char(ch))
}

/// Zero-width and invisible format characters dropped before tokenization so
/// they cannot break a term or evade a keyword. Covers the common evasion
/// vectors: soft hyphen, combining grapheme joiner, Arabic letter mark, Hangul
/// fillers, Khmer inherent vowels, Mongolian/variation selectors, the
/// zero-width space/joiners + bidi marks and overrides, the word-joiner /
/// invisible-operator / deprecated-format block, variation selectors, the
/// BOM/ZWNBSP, interlinear annotation anchors, and the supplementary variation
/// selectors. Curated common set, not the full Unicode
/// `Default_Ignorable_Code_Point` property.
fn is_ignored_format_char(ch: char) -> bool {
    matches!(ch as u32,
        0x00AD                 // SOFT HYPHEN
        | 0x034F               // COMBINING GRAPHEME JOINER
        | 0x061C               // ARABIC LETTER MARK
        | 0x115F..=0x1160      // HANGUL CHOSEONG/JUNGSEONG FILLER
        | 0x17B4..=0x17B5      // KHMER VOWEL INHERENT AQ/AA
        | 0x180B..=0x180F      // MONGOLIAN free variation selectors + vowel separator
        | 0x200B..=0x200F      // ZERO WIDTH SPACE/(NON-)JOINER, LRM/RLM
        | 0x202A..=0x202E      // bidi embedding / override controls
        | 0x2060..=0x206F      // WORD JOINER, invisible operators, deprecated format
        | 0xFE00..=0xFE0F      // VARIATION SELECTORS 1–16
        | 0xFEFF               // ZERO WIDTH NO-BREAK SPACE (BOM)
        | 0xFFF0..=0xFFF8      // reserved / interlinear annotation anchors
        | 0xE0100..=0xE01EF)   // VARIATION SELECTORS SUPPLEMENT
}

/// Combining marks dropped before tokenization so they cannot split a term or
/// disguise a keyword character. Curated common blocks (combining diacritics,
/// their extensions and supplement, marks for symbols, and half marks), not the
/// full Unicode `Mark` category.
fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F        // COMBINING DIACRITICAL MARKS
        | 0x1AB0..=0x1AFF      // … EXTENDED
        | 0x1DC0..=0x1DFF      // … SUPPLEMENT
        | 0x20D0..=0x20FF      // … FOR SYMBOLS
        | 0xFE20..=0xFE2F)     // COMBINING HALF MARKS
}

/// Han (CJK ideographic) scan ranges routed to Jieba: the main Unified
/// Ideographs block, Extension A, Compatibility Ideographs, and the
/// supplementary planes (Extensions B–H). Kana and Hangul are intentionally
/// excluded — they carry their own spacing/segmentation and go through ICU.
fn is_han_char(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF        // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0xF900..=0xFAFF      // CJK Compatibility Ideographs
        | 0x20000..=0x323AF)   // Extensions B–H (supplementary planes)
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
    let mut seen = HashSet::new();
    for line in content.lines() {
        let normalized = normalize_moderation_text(line);
        // `seen.insert` returns false for a duplicate; O(1) dedup that keeps the
        // Vec in first-seen order (imports are capped at 10k keywords, so a
        // linear `contains` scan here would be quadratic).
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
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
    let mut seen = HashSet::new();
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
        // O(1) first-seen dedup; see `parse_moderation_keywords_txt`.
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
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

    /// True when `text`, normalized through the matcher's own (keyword-seeded)
    /// analyzer, contains a keyword — the real request-scan path.
    fn blocks(matcher: &ModerationMatcher, text: &str) -> bool {
        matcher.find(&matcher.normalize(text)).is_some()
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
        // Chinese spans are word-segmented instead of split into characters.
        assert_eq!(normalize_moderation_text("你好\u{3000}世界"), "你好 世界");
        // Han-internal separators are removed before Chinese tokenization.
        assert_eq!(normalize_moderation_text("违.禁，词"), normalize_moderation_text("违禁词"));
        // NFKC folds fullwidth ASCII (letters, and fullwidth comma -> sep).
        assert_eq!(normalize_moderation_text("ＢＯＭＢ"), "bomb");
        assert_eq!(normalize_moderation_text("ｈｅｌｌｏ，ｗｏｒｌｄ"), "hello world");
        assert_eq!(normalize_moderation_text("bo\u{200b}mb"), "bomb");
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
        assert!(blocks(&matcher, "这句话包含敏感词内容"));
        // Punctuation inserted between the characters must not evade the match.
        assert!(blocks(&matcher, "这句话包含敏-感-词内容"));
        assert!(blocks(&matcher, "敏\u{3000}感\u{3000}词"));
    }

    #[test]
    fn cjk_keyword_does_not_match_inside_longer_words() {
        let matcher = matcher(&["口交"]);
        assert!(blocks(&matcher, "口交"));
        // 接口 + 交互 are strong dictionary words, so the seeded keyword loses
        // the segmentation contest and does not fire mid-word.
        assert!(!blocks(&matcher, "接口交互"));
    }

    #[test]
    fn cjk_separator_injection_does_not_create_subword_false_positive() {
        let matcher = matcher(&["口交"]);
        for text in ["口，交", "口。交", "口\u{3000}交", "口\u{200b}交"] {
            assert!(blocks(&matcher, text), "{text:?} should match");
        }
        assert!(!blocks(&matcher, "接口，交互"));
    }

    #[test]
    fn cjk_keyword_survives_context_induced_resegmentation() {
        // Regression for the tokenizer-evasion class: appending a character that
        // Jieba would otherwise merge with the keyword's last character (交好,
        // 交流, …) must not let the keyword slip through. Seeding keeps it whole.
        let matcher = matcher(&["口交"]);
        for text in [
            "口交",
            "请问口交好吗",
            "口交好",
            "他提到口交流程", // 交 would merge into 交流
            "口交视频",
            "我想口交",
            "关于口交的讨论",
            "口交口交口交",
        ] {
            assert!(blocks(&matcher, text), "{text:?} should be blocked");
        }
        // Genuine words that merely contain the character pair across a real
        // word boundary must still be allowed.
        for text in ["接口交互", "接口交流", "路口交通", "进出口交易", "窗口交换"] {
            assert!(!blocks(&matcher, text), "{text:?} should be allowed");
        }
    }

    #[test]
    fn han_internal_separator_run_collapses_in_linear_time() {
        // A long separator run between two Han characters must collapse to the
        // bare phrase. This also pins the O(n) filter: the previous quadratic
        // version took seconds on inputs this size.
        let noisy = format!("口{}交", "，".repeat(50_000));
        assert_eq!(normalize_moderation_text(&noisy), "口交");
        let matcher = matcher(&["口交"]);
        assert!(blocks(&matcher, &noisy));
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
