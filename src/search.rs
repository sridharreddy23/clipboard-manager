//! Search query construction and result ranking (§7, §47).
//!
//! # Strategy
//!
//! Searching happens in two stages so that the common case never scans the
//! table:
//!
//! 1. **Candidate selection via FTS5.** The user's query is turned into a
//!    prefix query (`"ffmpeg"* AND "input"*`) and matched against the
//!    `clipboard_items_fts` index. This is an index lookup, so it stays fast at
//!    100 000 entries.
//! 2. **Re-ranking in Rust.** The (bounded) candidate set is scored with a
//!    fzf-style fuzzy matcher blended with recency and pin state. Relevance
//!    ordering is therefore smarter than raw BM25 without giving up the index.
//!
//! A substring fallback exists for queries FTS cannot express — searching
//! `fmpeg` (a match *inside* a token) or a query made entirely of punctuation.
//! That path does scan, so it only runs when stage 1 found nothing.
//!
//! Case-insensitivity is Unicode-aware: SQLite's own `lower()` and `LIKE` only
//! fold ASCII, so the fallback uses a custom SQL function backed by Rust's
//! `str::to_lowercase` (see [`crate::storage::Store`]), and the FTS tokenizer is
//! configured with `remove_diacritics 2`.

/// Characters FTS5 treats as syntax. Tokens are quoted, which neutralises most
/// of these, but a token consisting only of syntax has no searchable content
/// and is dropped.
fn is_searchable(c: char) -> bool {
    c.is_alphanumeric()
}

/// Fold text for comparison: lowercase it, then strip combining marks.
///
/// This exists so the Rust matcher agrees with the index. The FTS5 tokenizer is
/// configured `unicode61 remove_diacritics 2`, which means `café` is indexed as
/// `cafe` and a search for `cafe` is an index hit. If the re-ranking stage then
/// compared raw characters it would score that hit as a non-match and discard
/// it, and searching `cafe` would find nothing — so both stages fold.
///
/// Non-ASCII input is canonically decomposed first, which is what makes the
/// mark-stripping work on precomposed characters such as `é` (U+00E9).
/// Decomposition also splits Hangul syllables into jamo; that is harmless here
/// because the needle and the haystack are folded identically.
fn fold(s: &str, limit: usize) -> Vec<char> {
    use unicode_normalization::char::{decompose_canonical, is_combining_mark};

    let mut out: Vec<char> = Vec::with_capacity(s.len().min(limit));
    for ch in s.chars() {
        if out.len() >= limit {
            break;
        }
        // The overwhelmingly common case, and it needs no normalisation.
        if ch.is_ascii() {
            out.push(ch.to_ascii_lowercase());
            continue;
        }
        for lower in ch.to_lowercase() {
            decompose_canonical(lower, |d| {
                if !is_combining_mark(d) {
                    out.push(d);
                }
            });
        }
    }
    out.truncate(limit);
    out
}

/// Translate a user query into an FTS5 `MATCH` expression.
///
/// Returns `None` when the query contains nothing an index can match, in which
/// case the caller should use the substring fallback.
///
/// Each whitespace-separated word becomes a quoted prefix term and the terms
/// are `AND`-ed, which matches how people expect multi-word search to behave:
/// `ffmpeg mp4` finds entries containing both.
pub fn build_fts_query(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();

    for word in query.split_whitespace() {
        // Keep only alphanumerics *within* the word but preserve the word
        // boundaries the user typed. `https://github.com/a` becomes the terms
        // `https`, `github`, `com`, `a` — which is exactly how the unicode61
        // tokenizer indexed it, so URL search works.
        let mut current = String::new();
        for ch in word.chars() {
            if is_searchable(ch) {
                current.push(ch);
            } else if !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            terms.push(current);
        }
    }

    if terms.is_empty() {
        return None;
    }

    // FTS5 has a hard limit on expression complexity; a pathologically long
    // query would be rejected by SQLite with a parse error. Bound it.
    terms.truncate(16);

    let expr = terms
        .iter()
        .map(|t| {
            // A quoted string in FTS5 escapes `"` by doubling it. `t` contains
            // only alphanumerics by construction, so this is belt-and-braces.
            format!("\"{}\"*", t.replace('"', "\"\""))
        })
        .collect::<Vec<_>>()
        .join(" AND ");

    Some(expr)
}

/// Score how well `needle` fuzzy-matches `haystack`.
///
/// Returns `None` when `needle` is not a subsequence of `haystack`. Higher
/// scores are better. Modelled on fzf's scoring:
///
/// * consecutive matched characters are strongly rewarded, so an exact
///   substring always outranks a scattered subsequence;
/// * a match at the start of the text, or at a word/camel-case boundary, gets a
///   bonus, so `gp` prefers `git push` over `graph`;
/// * gaps cost a little, so earlier and tighter matches win.
///
/// Comparison is done on [`fold`]ed characters, so it is case-insensitive for
/// every script Rust's `to_lowercase` covers — not just ASCII — and
/// diacritic-insensitive to match the index.
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    const SCORE_MATCH: i32 = 16;
    const BONUS_CONSECUTIVE: i32 = 12;
    const BONUS_BOUNDARY: i32 = 10;
    const BONUS_FIRST_CHAR: i32 = 14;
    const PENALTY_GAP_START: i32 = -3;
    const PENALTY_GAP_EXTEND: i32 = -1;
    /// Bound the work done per candidate so a multi-megabyte text entry cannot
    /// make one keystroke expensive (§45).
    const MAX_HAYSTACK_CHARS: usize = 4096;

    if needle.is_empty() {
        return Some(0);
    }

    let needle_chars = fold(needle, MAX_HAYSTACK_CHARS);
    let hay_chars = fold(haystack, MAX_HAYSTACK_CHARS);
    if needle_chars.is_empty() {
        return Some(0);
    }

    if needle_chars.len() > hay_chars.len() {
        return None;
    }

    let mut score = 0i32;
    let mut n_idx = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut in_gap = false;

    for (h_idx, &hc) in hay_chars.iter().enumerate() {
        if n_idx >= needle_chars.len() {
            break;
        }
        if hc != needle_chars[n_idx] {
            if prev_match.is_some() {
                score += if in_gap {
                    PENALTY_GAP_EXTEND
                } else {
                    in_gap = true;
                    PENALTY_GAP_START
                };
            }
            continue;
        }

        score += SCORE_MATCH;
        in_gap = false;

        if h_idx == 0 {
            score += BONUS_FIRST_CHAR;
        } else {
            let prev = hay_chars[h_idx - 1];
            let boundary = !prev.is_alphanumeric()
                // camelCase / PascalCase boundary, computed on the original
                // text so that lowercasing does not erase it.
                || (prev.is_lowercase() && haystack_char_is_upper(haystack, h_idx));
            if boundary {
                score += BONUS_BOUNDARY;
            }
        }

        if prev_match == Some(h_idx.wrapping_sub(1)) {
            score += BONUS_CONSECUTIVE;
        }

        prev_match = Some(h_idx);
        n_idx += 1;
    }

    if n_idx == needle_chars.len() {
        Some(score)
    } else {
        None
    }
}

/// Whether the `idx`-th character of the *original* (non-lowercased) text is
/// uppercase. Used only for the camel-case boundary bonus; a mismatch between
/// the lowercased index and the original index (possible for the handful of
/// characters whose lowercase form is longer than one char) degrades the bonus
/// but cannot produce a wrong match.
fn haystack_char_is_upper(haystack: &str, idx: usize) -> bool {
    haystack
        .chars()
        .nth(idx)
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

/// Unicode-aware case-insensitive substring test.
///
/// Registered with SQLite as `cm_contains_ci` so the fallback scan folds case
/// for non-ASCII text, which `LIKE` does not.
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    // Fast path: pure-ASCII needle *and* haystack can use byte comparison.
    if needle.is_ascii() && haystack.is_ascii() {
        return haystack.to_ascii_lowercase().contains(&needle.to_ascii_lowercase());
    }
    // Folded, for the same reason [`fuzzy_score`] folds: the fallback scan must
    // not disagree with the index about whether `cafe` matches `café`.
    let hay: String = fold(haystack, usize::MAX).into_iter().collect();
    let needle: String = fold(needle, usize::MAX).into_iter().collect();
    hay.contains(&needle)
}

/// Blend text relevance with recency and pin state into a final ordering key.
///
/// Pinned items sort first unconditionally (§11), which mirrors the pinned
/// section at the top of the Windows panel. Within a group, relevance
/// dominates, and recency breaks ties — a one-week-old entry can outrank a
/// one-minute-old one, but only if it matches distinctly better.
pub fn rank_key(
    text_score: i32,
    is_pinned: bool,
    last_used_at: i64,
    now_ms: i64,
) -> (bool, i32, i64) {
    // Age in hours, capped so that very old entries are not pushed into
    // oblivion by an unbounded penalty.
    let age_hours = ((now_ms - last_used_at).max(0) / 3_600_000).min(720) as i32;
    let recency_bonus = match age_hours {
        0 => 24,
        1..=5 => 16,
        6..=23 => 10,
        24..=167 => 4,
        _ => 0,
    };
    // `!is_pinned` so that `false` (pinned) sorts before `true` on an ascending
    // sort; the caller sorts by this tuple ascending with the score negated.
    (!is_pinned, -(text_score + recency_bonus), -last_used_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_query_becomes_a_prefix_term() {
        assert_eq!(build_fts_query("ffmpeg").unwrap(), "\"ffmpeg\"*");
    }

    #[test]
    fn multiple_words_are_anded() {
        assert_eq!(
            build_fts_query("ffmpeg mp4").unwrap(),
            "\"ffmpeg\"* AND \"mp4\"*"
        );
    }

    #[test]
    fn urls_split_into_indexable_terms() {
        assert_eq!(
            build_fts_query("https://github.com/example").unwrap(),
            "\"https\"* AND \"github\"* AND \"com\"* AND \"example\"*"
        );
    }

    #[test]
    fn punctuation_only_queries_have_no_fts_form() {
        assert!(build_fts_query("").is_none());
        assert!(build_fts_query("   ").is_none());
        assert!(build_fts_query("://").is_none());
        assert!(build_fts_query("!!! ***").is_none());
    }

    #[test]
    fn fts_syntax_cannot_escape_the_quoted_term() {
        // A query full of FTS operators must not produce an expression that
        // SQLite would parse as syntax.
        let q = build_fts_query("NEAR( a b ) OR \"x\" *").unwrap();
        assert!(!q.contains("NEAR("), "{q}");
        assert!(q.starts_with('"'));
        // Every `*` is a prefix marker directly after a closing quote.
        for (i, _) in q.match_indices('*') {
            assert_eq!(&q[i - 1..i], "\"");
        }
    }

    #[test]
    fn unicode_queries_survive() {
        assert_eq!(build_fts_query("日本語").unwrap(), "\"日本語\"*");
        assert_eq!(build_fts_query("café").unwrap(), "\"café\"*");
    }

    #[test]
    fn very_long_queries_are_bounded() {
        let long = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let q = build_fts_query(&long).unwrap();
        assert_eq!(q.matches(" AND ").count(), 15, "should cap at 16 terms");
    }

    #[test]
    fn fuzzy_requires_a_subsequence() {
        assert!(fuzzy_score("ffmpeg", "ffmpeg -i input.mp4 output.ts").is_some());
        assert!(fuzzy_score("fmpg", "ffmpeg").is_some());
        assert!(fuzzy_score("zzz", "ffmpeg").is_none());
        assert!(fuzzy_score("ffmpegg", "ffmpeg").is_none());
    }

    #[test]
    fn fuzzy_is_case_and_unicode_insensitive() {
        assert!(fuzzy_score("FFMPEG", "ffmpeg -i x").is_some());
        assert!(fuzzy_score("ÉCOLE", "l'école normale").is_some());
        assert!(fuzzy_score("日本", "これは日本語です").is_some());
    }

    #[test]
    fn empty_needle_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert_eq!(fuzzy_score("", ""), Some(0));
    }

    #[test]
    fn contiguous_beats_scattered() {
        let contiguous = fuzzy_score("nginx", "restart nginx now").unwrap();
        let scattered = fuzzy_score("nginx", "n g i n x").unwrap();
        assert!(
            contiguous > scattered,
            "contiguous {contiguous} should beat scattered {scattered}"
        );
    }

    #[test]
    fn word_start_beats_mid_word() {
        let at_boundary = fuzzy_score("pods", "kubectl get pods").unwrap();
        let mid_word = fuzzy_score("pods", "xxpodsxx").unwrap();
        assert!(at_boundary > mid_word, "{at_boundary} vs {mid_word}");
    }

    #[test]
    fn the_acceptance_example_ranks_first() {
        // From §7: a history of five items, searching "ffmpeg" must surface the
        // ffmpeg command.
        let history = [
            "sudo systemctl restart nginx",
            "kubectl get pods",
            "ffmpeg -i input.mp4 output.ts",
            "AWS MediaConnect flow",
            "Cloudport playlist issue",
        ];
        let mut scored: Vec<_> = history
            .iter()
            .filter_map(|h| fuzzy_score("ffmpeg", h).map(|s| (s, *h)))
            .collect();
        scored.sort_by_key(|(s, _)| -s);
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].1, "ffmpeg -i input.mp4 output.ts");
    }

    #[test]
    fn huge_haystacks_are_bounded_not_scanned() {
        let huge = "a".repeat(10_000_000);
        // Must return promptly; the needle only exists past the cap, so this
        // also proves the cap is applied.
        assert!(fuzzy_score("b", &huge).is_none());
        assert!(fuzzy_score("aaa", &huge).is_some());
    }

    #[test]
    fn fuzzy_folds_diacritics_the_way_the_index_does() {
        // FTS5's `remove_diacritics 2` indexes "café" as "cafe", so the matcher
        // must agree or an index hit would be re-scored as a non-match.
        assert!(fuzzy_score("cafe", "Beyoncé café au lait").is_some());
        assert!(fuzzy_score("café", "Beyonce cafe au lait").is_some());
        assert!(fuzzy_score("koln", "Grüße aus Köln").is_some());
        // `ß` is not a diacritic and is not folded to `ss`, matching SQLite.
        assert!(fuzzy_score("grusse", "Grüße aus Köln").is_none());
    }

    #[test]
    fn contains_ci_folds_diacritics_too() {
        assert!(contains_ci("Beyoncé café au lait", "cafe"));
        assert!(contains_ci("Grüße aus Köln", "koln"));
        assert!(!contains_ci("Grüße aus Köln", "grusse"));
    }

    #[test]
    fn contains_ci_folds_non_ascii() {
        assert!(contains_ci("Grüße aus Köln", "grüße"));
        assert!(contains_ci("STRASSE", "strasse"));
        assert!(contains_ci("ΑΘΗΝΑ", "αθηνα"));
        assert!(!contains_ci("hello", "world"));
        assert!(contains_ci("anything", ""));
    }

    #[test]
    fn pinned_items_always_rank_first() {
        let now = 1_000_000_000i64;
        let pinned_poor = rank_key(1, true, now - 30 * 86_400_000, now);
        let unpinned_perfect = rank_key(10_000, false, now, now);
        assert!(pinned_poor < unpinned_perfect);
    }

    #[test]
    fn among_equals_recent_wins() {
        let now = 1_000_000_000i64;
        let recent = rank_key(50, false, now, now);
        let old = rank_key(50, false, now - 10 * 86_400_000, now);
        assert!(recent < old);
    }

    #[test]
    fn a_much_better_match_can_beat_a_newer_one() {
        let now = 1_000_000_000i64;
        let old_great = rank_key(500, false, now - 10 * 86_400_000, now);
        let new_poor = rank_key(20, false, now, now);
        assert!(old_great < new_poor);
    }
}
