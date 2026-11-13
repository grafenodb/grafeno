//! How alike two texts, numbers or dates are: the measures entity
//! resolution compares fields with.
//!
//! Edit distances count characters (Unicode scalar values), not bytes.
//! Similarities go from 0 (nothing alike) to 1 (the same). Callers usually
//! compare normalized text ([`crate::normalize_name`]: lowercase, without
//! accents or punctuation); the measures themselves compare what they get.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The edits (insertions, deletions, substitutions) that turn `a` into `b`.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// [`levenshtein`] that also counts swapping two neighbors as one edit
/// (optimal string alignment: "jonh" and "john" are one edit apart).
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
    damerau_within(a, b, usize::MAX - 1).unwrap_or(usize::MAX)
}

/// [`damerau_levenshtein`] if it is at most `max`; `None` past it, found
/// without filling the whole table when the lengths alone rule it out.
pub fn damerau_within(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    // Three rows of the table, not all of it: a swap looks back two rows
    // and no further. The whole table took memory in the square of the
    // texts' length (a gigabyte for two texts of 12 000 characters), which
    // no query limit saw.
    let width = b.len() + 1;
    let mut before: Vec<usize> = vec![0; width];
    let mut previous: Vec<usize> = (0..width).collect();
    let mut current: Vec<usize> = vec![0; width];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before[j - 2] + 1);
            }
            current[j] = best;
        }
        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[b.len()];
    (distance <= max).then_some(distance)
}

/// `1 - distance / longer length`: 1 for equal texts, 0 for nothing kept.
pub fn edit_similarity(a: &str, b: &str) -> f64 {
    let longest = a.chars().count().max(b.chars().count());
    if longest == 0 {
        return 1.0;
    }
    1.0 - damerau_levenshtein(a, b) as f64 / longest as f64
}

/// Jaro similarity: characters in common within a window, and how many of
/// them are out of order. Suited to short strings such as names.
pub fn jaro(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut matched_b = vec![false; b.len()];
    let mut matches_a = Vec::new();
    for (i, ca) in a.iter().enumerate() {
        let from = i.saturating_sub(window);
        let to = (i + window + 1).min(b.len());
        for j in from..to {
            if !matched_b[j] && b[j] == *ca {
                matched_b[j] = true;
                matches_a.push(*ca);
                break;
            }
        }
    }
    let m = matches_a.len();
    if m == 0 {
        return 0.0;
    }
    let matches_b: Vec<char> = b
        .iter()
        .zip(&matched_b)
        .filter(|(_, matched)| **matched)
        .map(|(c, _)| *c)
        .collect();
    let transpositions = matches_a
        .iter()
        .zip(&matches_b)
        .filter(|(x, y)| x != y)
        .count()
        / 2;
    let m = m as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// Jaro-Winkler: [`jaro`] raised for a common prefix (up to four
/// characters, by 0.1 each), as names that start alike usually are alike.
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let j = jaro(a, b);
    let prefix = a
        .chars()
        .zip(b.chars())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count();
    j + prefix as f64 * 0.1 * (1.0 - j)
}

/// The character n-grams of each word, padded with spaces so that word
/// starts and ends count (`"ana"` → `"  a"`, `" an"`, `"ana"`, `"na "` for
/// n = 3), as a set.
pub fn ngrams(text: &str, n: usize) -> HashSet<String> {
    let n = n.max(1);
    let mut out = HashSet::new();
    for word in text.split_whitespace() {
        let padded: Vec<char> = std::iter::repeat_n(' ', n - 1)
            .chain(word.chars())
            .chain(std::iter::once(' '))
            .collect();
        for window in padded.windows(n) {
            out.insert(window.iter().collect());
        }
    }
    out
}

/// Shared n-grams over all n-grams of either (0 to 1).
pub fn jaccard(a: &str, b: &str, n: usize) -> f64 {
    let (x, y) = (ngrams(a, n), ngrams(b, n));
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    x.intersection(&y).count() as f64 / x.union(&y).count() as f64
}

/// Twice the shared n-grams over the n-grams of both (Sørensen-Dice).
pub fn dice(a: &str, b: &str, n: usize) -> f64 {
    let (x, y) = (ngrams(a, n), ngrams(b, n));
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    2.0 * x.intersection(&y).count() as f64 / (x.len() + y.len()) as f64
}

/// Shared words over all words (word order does not count: "garcia ana"
/// and "ana garcia" are the same).
pub fn token_jaccard(a: &str, b: &str) -> f64 {
    let x: HashSet<&str> = a.split_whitespace().collect();
    let y: HashSet<&str> = b.split_whitespace().collect();
    if x.is_empty() && y.is_empty() {
        return 1.0;
    }
    x.intersection(&y).count() as f64 / x.union(&y).count() as f64
}

/// The length of the longest common subsequence (characters in the same
/// order, not necessarily together).
pub fn lcs(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut previous = vec![0usize; b.len() + 1];
    for ca in &a {
        let mut current = vec![0usize; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            current[j + 1] = if ca == cb {
                previous[j] + 1
            } else {
                previous[j + 1].max(current[j])
            };
        }
        previous = current;
    }
    previous[b.len()]
}

/// `2 × lcs / (length of a + length of b)`.
pub fn lcs_similarity(a: &str, b: &str) -> f64 {
    let total = a.chars().count() + b.chars().count();
    if total == 0 {
        return 1.0;
    }
    2.0 * lcs(a, b) as f64 / total as f64
}

/// American Soundex of the first word: a letter and three digits for how
/// it sounds in English (`"Robert"` and `"Rupert"` are `R163`). Empty for
/// text without a letter.
pub fn soundex(text: &str) -> String {
    let letters: Vec<char> = text
        .chars()
        .flat_map(char::to_uppercase)
        .filter(char::is_ascii_alphabetic)
        .collect();
    let Some(&first) = letters.first() else {
        return String::new();
    };
    let code = |c: char| match c {
        'B' | 'F' | 'P' | 'V' => '1',
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => '2',
        'D' | 'T' => '3',
        'L' => '4',
        'M' | 'N' => '5',
        'R' => '6',
        'H' | 'W' => '-',
        _ => '0',
    };
    let mut out = String::from(first);
    let mut last = code(first);
    for &c in &letters[1..] {
        let digit = code(c);
        if digit == '-' {
            continue;
        }
        if digit != '0' && digit != last {
            out.push(digit);
            if out.len() == 4 {
                break;
            }
        }
        last = digit;
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

/// A key for how a Spanish word sounds, so that spellings pronounced the
/// same share it: `b`/`v`, `c`/`s`/`z` (before `e`, `i`), `ll`/`y`, `g`/`j`
/// (before `e`, `i`), `qu`/`k`/`c`, a silent `h` and doubled letters (`"Jiménez"` and `"Gimenes"` give the same key). For every word.
pub fn phonetic_es(text: &str) -> String {
    let normalized = crate::normalize_name(text);
    let mut words = Vec::new();
    for word in normalized.split(' ').filter(|w| !w.is_empty()) {
        let c: Vec<char> = word.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < c.len() {
            let next = c.get(i + 1).copied();
            let soft = matches!(next, Some('e') | Some('i'));
            let (sound, step): (&str, usize) = match c[i] {
                'h' => ("", 1),
                'v' | 'w' => ("b", 1),
                'z' => ("s", 1),
                'c' if next == Some('h') => ("ch", 2),
                'c' if soft => ("s", 1),
                'c' | 'k' => ("k", 1),
                'q' if next == Some('u') => ("k", 2),
                'q' => ("k", 1),
                'g' if soft => ("j", 1),
                'g' if next == Some('u') && matches!(c.get(i + 2), Some('e') | Some('i')) => {
                    ("g", 2)
                }
                'l' if next == Some('l') => ("y", 2),
                'y' if next.is_none_or(|n| !"aeiou".contains(n)) => ("i", 1),
                'x' => ("ks", 1),
                other => {
                    let mut buffer = [0u8; 4];
                    let s: &str = other.encode_utf8(&mut buffer);
                    out.push_str(s);
                    i += 1;
                    continue;
                }
            };
            out.push_str(sound);
            i += step;
        }
        // Doubled sounds (rr, ss, cc) collapse.
        let mut collapsed = String::new();
        for ch in out.chars() {
            if !collapsed.ends_with(ch) {
                collapsed.push(ch);
            }
        }
        words.push(collapsed);
    }
    words.join(" ")
}

/// How two dates (`YYYY-MM-DD`, or a longer timestamp that starts so)
/// agree: 1 the same day, 0.8 with day and month swapped (a common slip),
/// otherwise falling linearly to 0 at `days` apart. `None` if either is not
/// a date.
pub fn date_similarity(a: &str, b: &str, days: f64) -> Option<f64> {
    let parse = |s: &str| -> Option<(i64, i64, i64)> {
        let s = s.trim();
        let (y, rest) = s.split_once('-')?;
        let (m, rest) = rest.split_once('-')?;
        let d: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some((y.parse().ok()?, m.parse().ok()?, d.parse().ok()?))
    };
    let (x, y) = (parse(a)?, parse(b)?);
    if x == y {
        return Some(1.0);
    }
    if x.0 == y.0 && x.1 == y.2 && x.2 == y.1 {
        return Some(0.8);
    }
    if !(1..=12).contains(&x.1)
        || !(1..=12).contains(&y.1)
        || !(1..=31).contains(&x.2)
        || !(1..=31).contains(&y.2)
    {
        return None;
    }
    let (dx, dy) = (days_from_civil(x), days_from_civil(y));
    let apart = dx.abs_diff(dy) as f64;
    Some((1.0 - apart / days.max(1.0)).max(0.0))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil((y, m, d): (i64, i64, i64)) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// How two numbers agree: 1 when equal, falling linearly to 0 at
/// `tolerance` apart.
pub fn number_similarity(a: f64, b: f64, tolerance: f64) -> f64 {
    if a == b {
        return 1.0;
    }
    (1.0 - (a - b).abs() / tolerance.max(f64::MIN_POSITIVE)).max(0.0)
}

/// A way to compare two field values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "measure")]
pub enum Measure {
    /// Equal after normalizing (1 or 0).
    Exact,
    /// [`edit_similarity`] (Damerau-Levenshtein).
    Edit,
    JaroWinkler,
    Jaro,
    /// [`dice`] over n-grams (`n`, 3 by default).
    Dice {
        #[serde(default = "three")]
        n: usize,
    },
    Jaccard {
        #[serde(default = "three")]
        n: usize,
    },
    /// Shared words, in any order.
    Tokens,
    Lcs,
    /// Equal [`soundex`] codes (1 or 0).
    Soundex,
    /// Equal [`phonetic_es`] keys (1 or 0).
    PhoneticEs,
    /// [`date_similarity`] within `days` (365 by default).
    Date {
        #[serde(default = "year")]
        days: f64,
    },
    /// [`number_similarity`] within `tolerance` (1 by default).
    Number {
        #[serde(default = "one")]
        tolerance: f64,
    },
}

fn three() -> usize {
    3
}

fn year() -> f64 {
    365.0
}

fn one() -> f64 {
    1.0
}

impl Measure {
    /// Every measure by name, for parsing (`"jaro_winkler"`, `"dice"`, …).
    pub fn by_name(name: &str) -> Option<Self> {
        Some(
            match name.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
                "exact" => Self::Exact,
                "edit" | "levenshtein" | "damerau" | "damerau_levenshtein" => Self::Edit,
                "jaro_winkler" | "jarowinkler" => Self::JaroWinkler,
                "jaro" => Self::Jaro,
                "dice" | "sorensen_dice" | "trigram" => Self::Dice { n: 3 },
                "bigram" => Self::Dice { n: 2 },
                "jaccard" => Self::Jaccard { n: 3 },
                "tokens" | "token_set" | "words" => Self::Tokens,
                "lcs" => Self::Lcs,
                "soundex" => Self::Soundex,
                "phonetic_es" | "fonetico" => Self::PhoneticEs,
                "date" => Self::Date { days: 365.0 },
                "number" => Self::Number { tolerance: 1.0 },
                _ => return None,
            },
        )
    }

    /// How alike two texts are under this measure, from 0 to 1. Text
    /// measures normalize first; date and number measures that cannot read
    /// their input fall back to [`Measure::Exact`].
    pub fn compare(&self, a: &str, b: &str) -> f64 {
        let (x, y) = (crate::normalize_name(a), crate::normalize_name(b));
        let bool_score = |same: bool| if same { 1.0 } else { 0.0 };
        match *self {
            Self::Exact => bool_score(x == y),
            Self::Edit => edit_similarity(&x, &y),
            Self::JaroWinkler => jaro_winkler(&x, &y),
            Self::Jaro => jaro(&x, &y),
            Self::Dice { n } => dice(&x, &y, n),
            Self::Jaccard { n } => jaccard(&x, &y, n),
            Self::Tokens => token_jaccard(&x, &y),
            Self::Lcs => lcs_similarity(&x, &y),
            Self::Soundex => bool_score(!x.is_empty() && soundex(&x) == soundex(&y)),
            Self::PhoneticEs => bool_score(!x.is_empty() && phonetic_es(a) == phonetic_es(b)),
            Self::Date { days } => date_similarity(a, b, days).unwrap_or(bool_score(x == y)),
            Self::Number { tolerance } => {
                match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                    (Ok(p), Ok(q)) => number_similarity(p, q, tolerance),
                    _ => bool_score(x == y),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole table, as it was computed before: the reference.
    fn damerau_table(a: &str, b: &str) -> usize {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in rows.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in rows[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut best = (rows[i - 1][j] + 1)
                    .min(rows[i][j - 1] + 1)
                    .min(rows[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    best = best.min(rows[i - 2][j - 2] + 1);
                }
                rows[i][j] = best;
            }
        }
        rows[a.len()][b.len()]
    }

    #[test]
    fn damerau_in_three_rows_matches_the_whole_table() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2_000 {
            let mut word = |n: u64| -> String {
                (0..next() % n)
                    .map(|_| ['a', 'b', 'c', 'á', 'd'][(next() % 5) as usize])
                    .collect()
            };
            let (a, b) = (word(12), word(12));
            assert_eq!(
                damerau_levenshtein(&a, &b),
                damerau_table(&a, &b),
                "{a:?} {b:?}"
            );
        }
        for (a, b) in [
            ("", ""),
            ("ab", "ba"),
            ("abc", ""),
            ("", "xyz"),
            ("ca", "abc"),
        ] {
            assert_eq!(
                damerau_levenshtein(a, b),
                damerau_table(a, b),
                "{a:?} {b:?}"
            );
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn edit_distances() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("añejo", "anejo"), 1, "characters, not bytes");
        assert_eq!(levenshtein("jonh", "john"), 2);
        assert_eq!(damerau_levenshtein("jonh", "john"), 1);
        assert_eq!(damerau_within("graphene", "grafeno", 1), None);
        assert_eq!(damerau_within("graphene", "grafeno", 3), Some(3));
        assert!(close(edit_similarity("martha", "marhta"), 1.0 - 1.0 / 6.0));
    }

    #[test]
    fn jaro_and_winkler_match_the_published_examples() {
        assert!(close(jaro("martha", "marhta"), 0.944));
        assert!(close(jaro_winkler("martha", "marhta"), 0.961));
        assert!(close(jaro("dixon", "dicksonx"), 0.767));
        assert!(close(jaro_winkler("dixon", "dicksonx"), 0.813));
        assert!(close(jaro_winkler("dwayne", "duane"), 0.84));
        assert_eq!(jaro("abc", "xyz"), 0.0);
        assert_eq!(jaro_winkler("ana", "ana"), 1.0);
    }

    #[test]
    fn grams_words_and_subsequences() {
        let grams = ngrams("ana", 3);
        assert_eq!(grams.len(), 4);
        assert!(grams.contains("  a") && grams.contains("na "));
        assert_eq!(dice("ana garcia", "ana garcia", 3), 1.0);
        assert!(dice("ana garcia", "anna garcia", 3) > 0.7);
        assert!(jaccard("ana garcia", "pedro lopez", 3) < 0.1);
        assert_eq!(token_jaccard("garcia ana", "ana garcia"), 1.0);
        assert_eq!(lcs("abcbdab", "bdcaba"), 4);
    }

    #[test]
    fn sounds() {
        for (word, code) in [
            ("Robert", "R163"),
            ("Rupert", "R163"),
            ("Ashcraft", "A261"),
            ("Tymczak", "T522"),
            ("Pfister", "P236"),
            ("Lee", "L000"),
        ] {
            assert_eq!(soundex(word), code, "{word}");
        }
        assert_eq!(phonetic_es("Jiménez"), phonetic_es("Gimenes"));
        assert_eq!(phonetic_es("Valladolid"), phonetic_es("Balladolid"));
        assert_eq!(phonetic_es("Hernández"), phonetic_es("Ernandes"));
        assert_eq!(phonetic_es("Yolanda Quispe"), phonetic_es("Llolanda Kispe"));
        assert_ne!(phonetic_es("Mario"), phonetic_es("María"));
    }

    #[test]
    fn dates_and_numbers() {
        assert_eq!(
            date_similarity("1990-03-12", "1990-03-12", 365.0),
            Some(1.0)
        );
        assert_eq!(
            date_similarity("1990-03-12", "1990-12-03", 365.0),
            Some(0.8)
        );
        assert!(close(
            date_similarity("1990-03-12", "1990-03-22", 100.0).unwrap(),
            0.9
        ));
        assert_eq!(date_similarity("1990-03-12", "nope", 365.0), None);
        assert!(close(number_similarity(10.0, 11.0, 4.0), 0.75));
        assert_eq!(Measure::by_name("jaro-winkler"), Some(Measure::JaroWinkler));
        assert_eq!(
            Measure::JaroWinkler.compare("MARTHA", "Márhta"),
            jaro_winkler("martha", "marhta")
        );
        assert_eq!(
            Measure::Date { days: 365.0 }.compare("1990-03-12", "1990-12-03"),
            0.8
        );
    }
}
