//! Case-folding comparison (spec 5.6 rule 6): whether two relative paths name the
//! same entry under the rule of the directory that holds them.

use super::fold_table::FULL_FOLD;
use crate::sys::{self, Dir};
use std::io;
use std::path::is_separator;
use unicode_normalization::UnicodeNormalization;

/// How a directory compares names. [`case_rule`] reads it from the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CaseRule {
    /// Byte for byte: Linux, case-sensitive APFS, a Windows directory marked
    /// case-sensitive.
    Sensitive,
    /// NFD, full case folding, NFD again: case-insensitive APFS, which ignores the
    /// normalization form too, and a Linux directory with the casefold attribute.
    FoldNfd,
    /// Each UTF-16 unit upper-cased with the OS table, as
    /// `CompareStringOrdinal(a, b, TRUE)` does: NTFS and ReFS.
    #[cfg(windows)]
    Ordinal,
}

/// The rule of `dir`.
///
/// A filesystem without per-directory case support gets its fixed answer (see
/// spec 5.6 rule 6). Any other failure is returned; a caller that must decide
/// anyway treats it as case-insensitive, which refuses two names that might
/// become one rather than writing both.
pub fn case_rule(dir: &Dir) -> io::Result<CaseRule> {
    if sys::case_sensitive(dir)? {
        return Ok(CaseRule::Sensitive);
    }
    #[cfg(unix)]
    return Ok(CaseRule::FoldNfd);
    #[cfg(windows)]
    return Ok(CaseRule::Ordinal);
}

/// Whether `a` and `b` are one path under `rule`: the same number of components,
/// each pair equal under the rule. Components are split on `/`, and on Windows on
/// `\` too.
pub fn eq_on(rule: CaseRule, a: &str, b: &str) -> bool {
    folded(rule, a).eq(folded(rule, b))
}

/// A key that two paths share exactly when [`eq_on`] holds, for finding the
/// colliding paths of a set without comparing every pair (spec 7.3 rule 8).
/// Separators come out as `/`.
pub fn fold_key(rule: CaseRule, s: &str) -> String {
    folded(rule, s).collect()
}

/// `s` with every separator as `/` and every other character mapped by `rule`.
/// No rule maps a character to `/`, so components never merge.
fn folded(rule: CaseRule, s: &str) -> Box<dyn Iterator<Item = char> + '_> {
    let chars = s.chars().map(|c| if is_separator(c) { '/' } else { c });
    match rule {
        CaseRule::Sensitive => Box::new(chars),
        // Unicode's canonical caseless match (D145): folding can undo NFD.
        CaseRule::FoldNfd => Box::new(chars.nfd().flat_map(full_fold).nfd()),
        #[cfg(windows)]
        CaseRule::Ordinal => Box::new(chars.map(upcase)),
    }
}

/// `c` under full case folding: one character, or up to three for `ß` and the
/// like.
fn full_fold(c: char) -> Folded {
    match FULL_FOLD.binary_search_by_key(&c, |&(from, _)| from) {
        Ok(i) => Folded::Mapped(FULL_FOLD[i].1.chars()),
        Err(_) => Folded::Same(std::iter::once(c)),
    }
}

enum Folded {
    Same(std::iter::Once<char>),
    Mapped(std::str::Chars<'static>),
}

impl Iterator for Folded {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self {
            Self::Same(c) => c.next(),
            Self::Mapped(chars) => chars.next(),
        }
    }
}

/// A character outside the BMP is two surrogates in UTF-16, which the upcase
/// table leaves alone.
#[cfg(windows)]
fn upcase(c: char) -> char {
    u16::try_from(u32::from(c))
        .ok()
        .and_then(|unit| char::from_u32(u32::from(sys::upcase(unit))))
        .unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::CaseRule::*;
    use super::*;
    use crate::sys::test_dir::TestDir;
    use proptest::prelude::*;

    const RULES: &[CaseRule] = &[
        Sensitive,
        FoldNfd,
        #[cfg(windows)]
        Ordinal,
    ];

    /// ASCII, German sharp s, Greek sigma forms, Turkish dotted and dotless i,
    /// the Kelvin sign, precomposed and combining sequences, and separators.
    const CORPUS: &[&str] = &[
        "readme.md",
        "README.md",
        "Readme.MD",
        "\u{DF}", // ß
        "ss",
        "SS",
        "\u{1E9E}", // ẞ
        "\u{3A3}",  // Σ
        "\u{3C3}",  // σ
        "\u{3C2}",  // ς
        "i",
        "I",
        "\u{130}", // İ
        "\u{131}", // ı
        "k",
        "K",
        "\u{212A}", // Kelvin sign
        "\u{E9}",   // é
        "e\u{301}",
        "E\u{301}",
        "\u{C9}", // É
        "a/b",
        "A/B",
        "a\\b",
        "a",
        "ab",
    ];

    /// The corpus entries that are single names.
    fn names() -> impl Iterator<Item = &'static str> {
        CORPUS
            .iter()
            .copied()
            .filter(|n| !n.chars().any(is_separator))
    }

    #[test]
    fn fold_table_sorted() {
        assert!(FULL_FOLD.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn case_rule_per_os() {
        let tmp = TestDir::new();
        let rule = case_rule(&Dir::open(tmp.path()).unwrap()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(rule, Sensitive);
        #[cfg(target_os = "macos")]
        assert_eq!(rule, FoldNfd);
        #[cfg(windows)]
        assert_eq!(rule, Ordinal);
    }

    #[test]
    fn sensitive_compares_bytes() {
        assert!(!eq_on(Sensitive, "a", "A"));
        assert!(!eq_on(Sensitive, "\u{E9}", "e\u{301}"));
        assert!(eq_on(Sensitive, "a/b", "a/b"));
    }

    #[test]
    fn component_count_matters_for_every_rule() {
        for &rule in RULES {
            assert!(!eq_on(rule, "a/b", "a"), "{rule:?}");
            assert!(!eq_on(rule, "a/b", "ab"), "{rule:?}");
            assert!(!eq_on(rule, "a//b", "a/b"), "{rule:?}");
        }
    }

    #[test]
    fn fold_nfd_equalities() {
        for (a, b) in [
            ("README.md", "readme.md"),
            ("\u{E9}", "e\u{301}"),
            ("\u{C9}", "e\u{301}"),
            ("\u{1E9E}", "\u{DF}"),
            ("\u{DF}", "ss"),
            ("\u{1E9E}", "SS"),
            ("\u{130}", "i\u{307}"),
            ("\u{3A3}", "\u{3C2}"),
            ("\u{3C2}", "\u{3C3}"),
            ("\u{212A}", "k"),
            ("A/B", "a/b"),
        ] {
            assert!(eq_on(FoldNfd, a, b), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn fold_nfd_inequalities() {
        for (a, b) in [("\u{130}", "i"), ("\u{131}", "i"), ("a", "b")] {
            assert!(!eq_on(FoldNfd, a, b), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn fold_key_consistent_with_eq_on() {
        for &rule in RULES {
            for a in CORPUS {
                for b in CORPUS {
                    assert_eq!(
                        fold_key(rule, a) == fold_key(rule, b),
                        eq_on(rule, a, b),
                        "{rule:?}: {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_rule_makes_a_separator() {
        let mut buf = [0; 4];
        for c in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            if is_separator(c) {
                continue;
            }
            for &rule in RULES {
                let s = c.encode_utf8(&mut buf);
                assert!(
                    folded(rule, s).all(|f| f != '/'),
                    "{rule:?}: U+{:04X}",
                    u32::from(c)
                );
            }
        }
    }

    /// Creates each corpus name alone in `dir` and checks which corpus names the
    /// filesystem then finds, against `eq_on` under the directory's rule, and
    /// against `also` where it gives an answer.
    fn agrees_with_filesystem(dir: &std::path::Path, also: impl Fn(&str, &str) -> Option<bool>) {
        let rule = case_rule(&Dir::open(dir).unwrap()).unwrap();
        for a in names() {
            std::fs::write(dir.join(a), b"").unwrap();
            for b in names() {
                let found = dir.join(b).exists();
                assert_eq!(
                    eq_on(rule, a, b),
                    found,
                    "{a:?} vs {b:?}: filesystem says {found}"
                );
                if let Some(reference) = also(a, b) {
                    assert_eq!(eq_on(rule, a, b), reference, "{a:?} vs {b:?}: reference");
                }
            }
            std::fs::remove_file(dir.join(a)).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fold_nfd_matches_apfs() {
        let tmp = TestDir::new();
        if !crate::sys::test_dir::case_insensitive(tmp.path()) {
            eprintln!("skipped: the volume is case-sensitive");
            return;
        }
        agrees_with_filesystem(tmp.path(), |_, _| None);
    }

    #[cfg(windows)]
    #[test]
    fn ordinal_matches_ntfs() {
        let tmp = TestDir::new();
        agrees_with_filesystem(tmp.path(), |a, b| Some(sys::compare_ordinal(a, b)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_dir_case_sensitive_flag() {
        let tmp = TestDir::new();
        let path = tmp.mkdir_all("exact");
        if let Err(e) = sys::set_case_sensitive(&path, true) {
            eprintln!("skipped: cannot set the case-sensitive flag: {e}");
            return;
        }
        assert_eq!(case_rule(&Dir::open(&path).unwrap()).unwrap(), Sensitive);
        sys::set_case_sensitive(&path, false).unwrap();
        assert_eq!(case_rule(&Dir::open(&path).unwrap()).unwrap(), Ordinal);
    }

    /// The kernel's casefold is NFD plus full case folding too, so the corpus
    /// must agree. CI creates its ext4 image with `-O casefold` and sets
    /// `REMORA_EXPECT_CASEFOLD=1` there, so the test cannot skip on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_casefold_flag() {
        let tmp = TestDir::new();
        let path = tmp.mkdir_all("folded");
        let dir = Dir::open(&path).unwrap();
        match sys::set_casefold(&dir) {
            Ok(()) => {}
            Err(e) if std::env::var("REMORA_EXPECT_CASEFOLD").as_deref() != Ok("1") => {
                eprintln!("skipped: no casefold on this filesystem: {e}");
                return;
            }
            Err(e) => panic!("REMORA_EXPECT_CASEFOLD=1 but the flag was refused: {e}"),
        }
        assert_eq!(case_rule(&dir).unwrap(), FoldNfd);
        agrees_with_filesystem(&path, |_, _| None);
    }

    proptest! {
        #[test]
        fn eq_on_reflexive_and_symmetric(a in any::<String>(), b in any::<String>()) {
            for &rule in RULES {
                prop_assert!(eq_on(rule, &a, &a));
                prop_assert_eq!(eq_on(rule, &a, &b), eq_on(rule, &b, &a));
            }
        }

        #[test]
        fn ascii_case_only_matters_when_sensitive(pre in any::<String>(), post in any::<String>()) {
            let (lower, upper) = (format!("{pre}a{post}"), format!("{pre}A{post}"));
            for &rule in RULES {
                prop_assert_eq!(eq_on(rule, &lower, &upper), rule != Sensitive, "{:?}", rule);
            }
        }
    }
}
