//! Subsequence fuzzy scoring for type-to-jump navigation.

fn folded(value: &str) -> Vec<char> {
    crate::model::caseless_key(value).chars().collect()
}

/// Score how well `query` fuzzy-matches `text` (case-insensitive).
/// Higher is better. `None` if `query` is not a subsequence of `text`.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let query = folded(query);
    score_folded(&query, text)
}

fn score_folded(query: &[char], text: &str) -> Option<i32> {
    if query.is_empty() {
        return None;
    }

    let mut qi = 0;
    let mut total = 0i32;
    let mut prev_match: Option<usize> = None;
    let mut first = true;
    let mut prev_char: Option<char> = None;
    let mut t_len = 0usize;

    for (ti, tc) in crate::model::caseless_key(text).chars().enumerate() {
        t_len = ti + 1;
        if qi < query.len() && tc == query[qi] {
            let mut points = 1;
            if ti == 0 {
                points += 8;
            } else if let Some(prev) = prev_char
                && matches!(prev, ' ' | '-' | '_' | '/' | '.' | ':')
            {
                points += 6;
            }
            if let Some(p) = prev_match {
                if ti == p + 1 {
                    points += 10;
                } else {
                    points -= (ti - p - 1).min(5) as i32;
                }
            }
            if first {
                total += 4i32.saturating_sub((ti as i32).min(4));
                first = false;
            }
            total += points;
            prev_match = Some(ti);
            qi += 1;
        }
        prev_char = Some(tc);
    }

    if qi < query.len() {
        return None;
    }
    // Slight preference for shorter titles when the match is otherwise equal.
    total += 2i32.saturating_sub((t_len as i32 / 20).min(2));
    Some(total)
}

/// Index of the best-scoring title, or `None` if nothing matches.
/// Ties keep the earliest index.
pub fn best_index<'a, I>(query: &str, titles: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a str>,
{
    let query = folded(query);
    let mut best: Option<(i32, usize)> = None;
    for (i, title) in titles.into_iter().enumerate() {
        let Some(s) = score_folded(&query, title) else {
            continue;
        };
        match best {
            Some((bs, _)) if s <= bs => {}
            _ => best = Some((s, i)),
        }
    }
    best.map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prefix_beats_later_match() {
        let titles = ["later milk", "milk", "milkshake"];
        assert_eq!(best_index("milk", titles), Some(1));
    }

    #[test]
    fn subsequence_matches() {
        assert!(score("mlk", "milk").is_some());
        assert!(score("xyz", "milk").is_none());
    }

    #[test]
    fn case_insensitive() {
        assert!(score("Mi", "milk").is_some());
        assert_eq!(best_index("MI", ["alpha", "Milk", "beta"]), Some(1));
    }

    #[test]
    fn case_insensitive_matching_uses_unicode_equivalence() {
        assert!(score("MASSE", "Maße").is_some());
        assert!(score("é", "e\u{301}").is_some());
        assert!(score("strs", "Straße").is_some());
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert_eq!(best_index("", ["a", "b"]), None);
        assert!(score("", "a").is_none());
    }

    #[test]
    fn consecutive_beats_scattered() {
        let a = score("ab", "ab x").unwrap();
        let b = score("ab", "a x b").unwrap();
        assert!(a > b);
    }
}
