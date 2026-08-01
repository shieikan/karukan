//! Katakana form rewriter — produces full-width and half-width katakana variants.
//!
//! Only applies to candidates that consist entirely of hiragana or full-width
//! katakana (i.e. the reading itself / pure-kana fallbacks). Model output
//! candidates that mix kanji with kana are NOT rewritten — converting
//! `愛してる` into `愛ｼﾃﾙ` would be nonsense.
//!
//! For a pure-kana candidate, this rewriter emits:
//! - The full-width katakana form (only if the candidate has hiragana)
//! - The half-width katakana form (only if it differs from the candidate)
//!
//! Variants identical to the original or to each other are not emitted.
//!
//! Each emitted variant carries a mozc-style width annotation
//! (`[全]カタカナ` / `[半]カタカナ`) so the candidate window can label them.

use crate::kana::{hiragana_to_katakana, katakana_to_half_width};

use super::{RewriteOutput, Rewriter};

/// Annotation shown on full-width katakana variants.
const FULL_KATAKANA_DESC: &str = "[全]カタカナ";

/// Annotation shown on half-width katakana variants.
const HALF_KATAKANA_DESC: &str = "[半]カタカナ";

/// Rewriter that produces full-width and half-width katakana variants.
pub struct HalfWidthKatakanaRewriter;

fn contains_hiragana(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{3041}'..='\u{3096}'))
}

/// True if every character is in the hiragana or katakana block (including
/// the prolonged sound mark, sokuon, dakuten/handakuten, and small kana).
fn is_pure_kana(text: &str) -> bool {
    text.chars()
        .all(|c| matches!(c, '\u{3041}'..='\u{3096}' | '\u{30A0}'..='\u{30FF}'))
}

fn is_decimal_prefix_digit(c: char) -> bool {
    matches!(c, '0'..='9' | '\u{FF10}'..='\u{FF19}')
}

fn is_decimal_prefix_suffix_char(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'..='\u{3096}'
            | '\u{3099}'..='\u{309F}'
            | '\u{30A1}'..='\u{30FA}'
            | '\u{30FC}'..='\u{30FF}'
    )
}

fn decimal_prefix_parts(text: &str) -> Option<(&str, &str)> {
    let prefix_end = text
        .char_indices()
        .take_while(|&(_, c)| is_decimal_prefix_digit(c))
        .last()
        .map_or(0, |(index, c)| index + c.len_utf8());
    if prefix_end == 0 {
        return None;
    }

    let (prefix, suffix) = text.split_at(prefix_end);
    if suffix.is_empty() || !suffix.chars().all(is_decimal_prefix_suffix_char) {
        return None;
    }

    Some((prefix, suffix))
}

impl Rewriter for HalfWidthKatakanaRewriter {
    fn name(&self) -> &'static str {
        "katakana_form"
    }

    fn rewrite(&self, candidate: &str) -> Vec<RewriteOutput> {
        let (prefix, suffix) = if let Some(parts) = decimal_prefix_parts(candidate) {
            parts
        } else {
            if candidate.is_empty() || !is_pure_kana(candidate) {
                return Vec::new();
            }
            ("", candidate)
        };

        let mut out: Vec<RewriteOutput> = Vec::new();

        let full_kata_suffix = if contains_hiragana(suffix) {
            hiragana_to_katakana(suffix)
        } else {
            suffix.to_string()
        };
        let full_kata = if prefix.is_empty() {
            full_kata_suffix.clone()
        } else {
            format!("{prefix}{full_kata_suffix}")
        };

        // Full-width katakana (only if candidate contains hiragana)
        if contains_hiragana(suffix) && full_kata != candidate {
            out.push((full_kata.clone(), Some(FULL_KATAKANA_DESC.to_string())));
        }

        // Half-width katakana
        let half_suffix = katakana_to_half_width(&full_kata_suffix);
        let half = if prefix.is_empty() {
            half_suffix
        } else {
            format!("{prefix}{half_suffix}")
        };
        if half != candidate && !out.iter().any(|(s, _)| s == &half) {
            out.push((half, Some(HALF_KATAKANA_DESC.to_string())));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewriter::test_util::{desc, texts};

    #[test]
    fn empty_input_returns_empty() {
        let r = HalfWidthKatakanaRewriter;
        assert!(r.rewrite("").is_empty());
    }

    #[test]
    fn pure_kanji_returns_empty() {
        let r = HalfWidthKatakanaRewriter;
        assert!(r.rewrite("競技").is_empty());
    }

    #[test]
    fn pure_ascii_returns_empty() {
        let r = HalfWidthKatakanaRewriter;
        assert!(r.rewrite("abc").is_empty());
    }

    #[test]
    fn hiragana_emits_full_and_half() {
        let r = HalfWidthKatakanaRewriter;
        assert_eq!(
            texts(&r.rewrite("あいう")),
            vec!["アイウ".to_string(), "ｱｲｳ".to_string()]
        );
    }

    #[test]
    fn full_katakana_emits_half_only() {
        let r = HalfWidthKatakanaRewriter;
        assert_eq!(texts(&r.rewrite("アイウ")), vec!["ｱｲｳ".to_string()]);
    }

    #[test]
    fn decimal_prefix_preserves_spelling_and_emits_katakana_suffix_variants() {
        let r = HalfWidthKatakanaRewriter;
        let fixtures: [(&str, Vec<&str>); 4] = [
            ("10けん", vec!["10ケン", "10ｹﾝ"]),
            ("１０けん", vec!["１０ケン", "１０ｹﾝ"]),
            ("1０けん", vec!["1０ケン", "1０ｹﾝ"]),
            ("10ケーキ", vec!["10ｹｰｷ"]),
        ];

        for (candidate, expected) in fixtures {
            assert_eq!(
                texts(&r.rewrite(candidate)),
                expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "unexpected decimal-prefix rewrite for {candidate:?}"
            );
        }
    }

    #[test]
    fn mixed_width_decimal_prefix_preserves_codepoints_and_rejects_suffix_boundaries() {
        let r = HalfWidthKatakanaRewriter;

        let allowed_endpoints: [(u32, Vec<&str>); 8] = [
            (0x3041, vec!["10ァ", "10ｧ"]),
            (0x3096, vec!["10ヶ"]),
            (0x3099, vec![]),
            (0x309F, vec![]),
            (0x30A1, vec!["10ｧ"]),
            (0x30FA, vec![]),
            (0x30FC, vec!["10ｰ"]),
            (0x30FF, vec![]),
        ];
        for (code_point, expected) in allowed_endpoints {
            let suffix = char::from_u32(code_point).expect("valid Unicode endpoint");
            let suffix_text = suffix.to_string();
            let candidate = format!("10{suffix}");
            let (prefix, parsed_suffix) = decimal_prefix_parts(&candidate)
                .unwrap_or_else(|| panic!("allowed suffix endpoint rejected: U+{code_point:04X}"));
            assert_eq!(prefix, "10", "prefix changed for U+{code_point:04X}");
            assert_eq!(
                parsed_suffix, suffix_text,
                "suffix changed for U+{code_point:04X}"
            );
            assert_eq!(
                texts(&r.rewrite(&candidate)),
                expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "unexpected rewrite for allowed suffix endpoint U+{code_point:04X}"
            );
        }

        for code_point in [0x3040, 0x3097, 0x3098, 0x30A0, 0x30FB, 0x3100] {
            let suffix = char::from_u32(code_point).expect("valid Unicode boundary");
            let candidate = format!("10{suffix}");
            assert!(
                decimal_prefix_parts(&candidate).is_none(),
                "rejected suffix boundary was accepted: U+{code_point:04X}"
            );
            assert!(
                r.rewrite(&candidate).is_empty(),
                "unexpected rewrite for rejected suffix boundary U+{code_point:04X}"
            );
        }

        for candidate in ["10けん!", "10.けん", "10けん。"] {
            assert!(
                decimal_prefix_parts(candidate).is_none(),
                "punctuation suffix was accepted: {candidate:?}"
            );
            assert!(
                r.rewrite(candidate).is_empty(),
                "unexpected rewrite for {candidate:?}"
            );
        }

        for candidate in [
            "10゠けん",
            "10・けん",
            "10ケン・",
            "10,けん",
            "10、けん",
            "",
            "10",
            "abc",
            "10abc",
            "10ｹﾝ",
            "10件",
            "10けんabc",
        ] {
            assert!(
                r.rewrite(candidate).is_empty(),
                "unexpected rewrite for {candidate:?}"
            );
        }

        let legacy_contrasts: [(&str, Vec<&str>); 2] = [("゠", vec![]), ("・", vec!["･"])];
        for (zero_prefix, expected) in legacy_contrasts {
            assert!(
                is_pure_kana(zero_prefix),
                "legacy pure-kana gate changed for {zero_prefix:?}"
            );
            assert_eq!(
                texts(&r.rewrite(zero_prefix)),
                expected.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                "legacy zero-prefix output changed for {zero_prefix:?}"
            );

            let decimal_prefixed = format!("10{zero_prefix}");
            assert!(
                decimal_prefix_parts(&decimal_prefixed).is_none(),
                "decimal-only rejection boundary was accepted for {zero_prefix:?}"
            );
            assert!(
                r.rewrite(&decimal_prefixed).is_empty(),
                "unexpected decimal-prefixed rewrite for {zero_prefix:?}"
            );
        }
    }

    #[test]
    fn voiced_dakuten_expands() {
        let r = HalfWidthKatakanaRewriter;
        assert_eq!(
            texts(&r.rewrite("がっこう")),
            vec!["ガッコウ".to_string(), "ｶﾞｯｺｳ".to_string()]
        );
    }

    #[test]
    fn mixed_kanji_kana_returns_empty() {
        let r = HalfWidthKatakanaRewriter;
        // Mixed kanji + kana inputs (typical model output) must NOT be rewritten:
        // turning `愛してる` into `愛ｼﾃﾙ` is nonsense.
        assert!(r.rewrite("競技プログラミング").is_empty());
        assert!(r.rewrite("愛してる").is_empty());
        assert!(r.rewrite("漢字あ").is_empty());
    }

    #[test]
    fn does_not_emit_self() {
        let r = HalfWidthKatakanaRewriter;
        let out = r.rewrite("ｱｲｳ");
        assert!(!out.iter().any(|(s, _)| s == "ｱｲｳ"));
    }

    #[test]
    fn descriptions_match_width_form() {
        // Mozc-style width annotations: full-width katakana → `[全]カタカナ`,
        // half-width katakana → `[半]カタカナ`.
        let r = HalfWidthKatakanaRewriter;
        let out = r.rewrite("あいう");
        assert_eq!(desc(&out, "アイウ"), Some(FULL_KATAKANA_DESC.to_string()));
        assert_eq!(desc(&out, "ｱｲｳ"), Some(HALF_KATAKANA_DESC.to_string()));

        let out = r.rewrite("アイウ");
        assert_eq!(desc(&out, "ｱｲｳ"), Some(HALF_KATAKANA_DESC.to_string()));
    }
}
