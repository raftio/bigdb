// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! SQL's `LIKE`, as a matcher over one key at a time.
//!
//! Two wildcards and one escape: `%` for any run of characters including none, `_` for exactly
//! one, and `\` to mean the next character literally. **No regular expressions.** That is not a
//! subset chosen to save work - it is the whole of what `LIKE` is, and a regex engine in the
//! read path would be a dependency, a compile step and a class of pathological pattern, all to
//! implement two characters.
//!
//! # Where this runs
//!
//! Over the *key dictionary* of one field, not over records. A keyed column stores each distinct
//! string once and a bitmap of the records holding it, so `country LIKE 'G%'` is: walk the
//! field's keys, keep the ones that match, union their bitmaps. The cost is the field's
//! cardinality, and it is paid once - where a row engine would pay it per record.
//!
//! An obvious refinement is left undone on purpose: a pattern whose wildcards are all at the
//! end - `'G%'` - could seek the dictionary's B-tree to the prefix and stop at the first key
//! past it, turning the scan into a range. That is worth doing when a field's cardinality gets
//! big enough to notice; it is not worth doing before there is a measurement saying so, and it
//! changes nothing about what this file answers.

/// One element of a parsed pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Elem {
    /// A character that must appear exactly.
    Lit(char),
    /// `_`: exactly one character, whichever it is.
    One,
    /// `%`: any run, including an empty one.
    Any,
}

/// Whether `text` matches `pattern`, under SQL's `LIKE` rules.
///
/// `fold` is what separates `ILIKE` from `LIKE`: it lowercases both sides before comparing, and
/// nothing else about the two differs.
pub fn matches(pattern: &str, text: &str, fold: bool) -> bool {
    let p = parse(pattern, fold);
    let t: Vec<char> = match fold {
        true => text.to_lowercase().chars().collect(),
        false => text.chars().collect(),
    };
    run(&p, &t)
}

/// The pattern as elements, with `\` consumed as an escape.
///
/// A trailing `\` is a literal backslash rather than an error: this runs inside a query that has
/// already been accepted, and refusing here would mean a read failing on the shape of a string
/// somebody typed. Every engine reads it one of these two ways and neither is surprising.
fn parse(pattern: &str, fold: bool) -> Vec<Elem> {
    let lowered;
    let pattern = match fold {
        true => {
            lowered = pattern.to_lowercase();
            lowered.as_str()
        }
        false => pattern,
    };
    let mut out = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push(Elem::Any),
            '_' => out.push(Elem::One),
            '\\' => out.push(Elem::Lit(chars.next().unwrap_or('\\'))),
            other => out.push(Elem::Lit(other)),
        }
    }
    out
}

/// The match itself: greedy, with one backtrack point.
///
/// The textbook wildcard walk rather than a general backtracking search, and the difference
/// matters here. Only the most recent `%` is ever reconsidered, so a pattern like `'%a%a%a%b'`
/// against a long run of `a`s costs the length of the text times the number of wildcards,
/// instead of the exponential a naive recursion would pay. A read path is not the place to
/// discover that a user's pattern has a bad shape.
fn run(p: &[Elem], t: &[char]) -> bool {
    let (mut i, mut j) = (0usize, 0usize);
    // Where to resume from if the run this `%` swallowed turns out to be too short.
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while i < t.len() {
        match p.get(j) {
            Some(Elem::One) => {
                i += 1;
                j += 1;
            }
            Some(Elem::Lit(c)) if *c == t[i] => {
                i += 1;
                j += 1;
            }
            Some(Elem::Any) => {
                star = Some(j);
                mark = i;
                j += 1;
            }
            // A mismatch: give the last `%` one more character and try again from there.
            _ => match star {
                Some(sj) => {
                    j = sj + 1;
                    mark += 1;
                    i = mark;
                }
                None => return false,
            },
        }
    }
    // Trailing wildcards can still match nothing, which is what lets `'GB%'` match `GB`.
    p[j..].iter().all(|e| *e == Elem::Any)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_with_no_wildcard_is_an_equality() {
        assert!(matches("GB", "GB", false));
        assert!(!matches("GB", "GBR", false));
        assert!(!matches("GB", "G", false));
        assert!(!matches("GB", "gb", false));
    }

    #[test]
    fn percent_matches_any_run_including_an_empty_one() {
        assert!(matches("G%", "GB", false));
        assert!(matches("G%", "G", false));
        assert!(matches("%B", "GB", false));
        assert!(matches("%", "", false));
        assert!(matches("%%", "anything", false));
        assert!(!matches("G%", "AG", false));
    }

    #[test]
    fn underscore_matches_exactly_one_character() {
        assert!(matches("G_", "GB", false));
        assert!(!matches("G_", "G", false));
        assert!(!matches("G_", "GBR", false));
        assert!(matches("_B_", "aBc", false));
    }

    #[test]
    fn a_backslash_makes_the_next_character_literal() {
        assert!(matches("100\\%", "100%", false));
        assert!(!matches("100\\%", "100x", false));
        assert!(matches("a\\_b", "a_b", false));
        assert!(!matches("a\\_b", "axb", false));
        // A trailing backslash is a backslash, which is one of the two readings every engine
        // takes and the one that cannot fail a query.
        assert!(matches("a\\", "a\\", false));
    }

    #[test]
    fn folding_is_the_only_difference_between_like_and_ilike() {
        assert!(matches("gb", "GB", true));
        assert!(matches("G%", "gb", true));
        assert!(!matches("gb", "GB", false));
    }

    /// The case the greedy walk exists for: only the last `%` is reconsidered, so this is
    /// linear-ish rather than exponential. It is here as a guard on the algorithm, not on the
    /// answer - a naive recursive matcher would hang rather than fail.
    #[test]
    fn many_wildcards_over_a_long_run_do_not_blow_up() {
        let text = "a".repeat(64);
        assert!(!matches("%a%a%a%a%a%b", &text, false));
        assert!(matches("%a%a%a%a%a%a", &text, false));
    }

    #[test]
    fn multi_byte_characters_count_as_one() {
        assert!(matches("_", "é", false));
        assert!(matches("caf_", "café", false));
        assert!(matches("%é", "café", false));
    }

    #[test]
    fn an_empty_pattern_matches_only_an_empty_string() {
        assert!(matches("", "", false));
        assert!(!matches("", "a", false));
    }
}
