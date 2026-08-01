//! Integration tests for the rewriter chain in conversion candidates.
//!
//! No kanji model is loaded — these tests exercise only the rewriter path,
//! so they verify behaviour that holds independently of model output.

use std::collections::HashSet;
use std::io::Write;

use karukan_engine::{Dictionary, RewriterChain};

use super::*;

// ---------- helpers ----------

/// Engine in Composing state with the kanji model explicitly disabled.
fn composing_engine(reading: &str) -> InputMethodEngine {
    let mut engine = InputMethodEngine::new();
    engine.input_buf.text = reading.to_string();
    engine.input_buf.cursor_pos = reading.chars().count();
    engine.state = InputState::Composing {
        preedit: Preedit::new(),
        romaji_buffer: String::new(),
    };
    engine.converters.kanji = None;
    engine
}

/// Run `build_conversion_candidates` and return just the candidate texts.
fn conversion_texts(reading: &str) -> Vec<String> {
    let mut engine = composing_engine(reading);
    engine
        .build_conversion_candidates(reading, 9, false)
        .into_iter()
        .map(|c| c.text)
        .collect()
}

/// Build a one-entry user dictionary as a temp JSON file and load it.
fn user_dict_with(reading: &str, surface: &str) -> Dictionary {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    let json = format!(
        r#"[{{"reading":"{reading}","candidates":[{{"surface":"{surface}","score":1.0}}]}}]"#
    );
    tmp.write_all(json.as_bytes()).unwrap();
    tmp.flush().unwrap();
    Dictionary::build_from_json(tmp.path()).unwrap()
}

fn user_dict_with_surfaces(reading: &str, surfaces: &[&str]) -> Dictionary {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    let candidates = surfaces
        .iter()
        .map(|surface| format!(r#"{{"surface":"{surface}","score":1.0}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"[{{"reading":"{reading}","candidates":[{candidates}]}}]"#);
    tmp.write_all(json.as_bytes()).unwrap();
    tmp.flush().unwrap();
    Dictionary::build_from_json(tmp.path()).unwrap()
}

/// Drive the engine by typing a string of characters.
fn type_string(engine: &mut InputMethodEngine, s: &str) {
    for ch in s.chars() {
        engine.process_key(&press(ch));
    }
}

/// Texts in the conversion-state candidate list.
fn conversion_state_texts(engine: &InputMethodEngine) -> Vec<String> {
    engine
        .state()
        .candidates()
        .map(|cl| cl.candidates().iter().map(|c| c.text.clone()).collect())
        .unwrap_or_default()
}

/// Texts from the most recent ShowCandidates action (auto-suggest path).
fn auto_suggest_texts(result: &EngineResult) -> Vec<String> {
    result
        .actions
        .iter()
        .find_map(|a| match a {
            EngineAction::ShowCandidates(list) => {
                Some(list.candidates().iter().map(|c| c.text.clone()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

#[track_caller]
fn assert_contains(texts: &[String], expected: &str) {
    assert!(
        texts.iter().any(|t| t == expected),
        "expected `{expected}` in candidates, got: {texts:?}"
    );
}

#[track_caller]
fn assert_not_contains(texts: &[String], forbidden: &str) {
    assert!(
        !texts.iter().any(|t| t == forbidden),
        "`{forbidden}` should NOT be in candidates, got: {texts:?}"
    );
}

// ---------- half-width katakana variants ----------

#[test]
fn single_hiragana_hides_half_width_katakana_without_a_semantic_page() {
    assert_not_contains(&conversion_texts("あ"), "ｱ");
}

#[test]
fn hiragana_word_hides_half_width_katakana_without_a_semantic_page() {
    assert_not_contains(&conversion_texts("がっこう"), "ｶﾞｯｺｳ");
}

#[test]
fn short_word_hides_pure_script_variants_without_a_semantic_page() {
    let texts = conversion_texts("がっこう");
    assert_not_contains(&texts, "がっこう");
    assert_not_contains(&texts, "ガッコウ");
    assert_not_contains(&texts, "ｶﾞｯｺｳ");
}

#[test]
fn pure_script_variants_follow_a_full_semantic_page() {
    let mut engine = composing_engine("がっこう");
    engine.dicts.user = Some(user_dict_with_surfaces(
        "がっこう",
        &[
            "ガッコウ",
            "学校00",
            "学校01",
            "学校02",
            "学校03",
            "学校04",
            "学校05",
            "学校06",
            "学校07",
            "学校08",
        ],
    ));
    let candidates = engine.build_conversion_candidates("がっこう", 9, false);
    let texts: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.text.as_str())
        .collect();

    assert_not_contains(
        &texts
            .iter()
            .map(|text| (*text).to_string())
            .collect::<Vec<_>>(),
        "がっこう",
    );
    for variant in ["ガッコウ", "ｶﾞｯｺｳ"] {
        let index = texts
            .iter()
            .position(|text| *text == variant)
            .unwrap_or_else(|| panic!("missing {variant}: {texts:?}"));
        assert!(index >= 9, "{variant} must start after page one: {texts:?}");
    }
}

#[test]
fn sentence_like_conversion_omits_pure_script_variants() {
    let texts = conversion_texts("きょうはいいてんき");
    assert_not_contains(&texts, "きょうはいいてんき");
    assert_not_contains(&texts, "キョウハイイテンキ");
    assert_not_contains(&texts, "ｷｮｳﾊｲｲﾃﾝｷ");
}

#[test]
fn sentence_like_explicit_conversion_still_enters_conversion_state() {
    let mut engine = composing_engine("きょうはいいてんき");

    let result = engine.start_conversion(false);

    assert!(matches!(engine.state(), InputState::Conversion { .. }));
    assert!(engine.pending_async_request.is_some());
    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::HideCandidates))
    );
}

#[test]
fn sentence_live_katakana_is_not_reinserted_into_explicit_candidates() {
    let reading = "きょうはいいてんき";
    let mut engine = composing_engine(reading);
    engine.live.text = "キョウハイイテンキ".to_string();

    engine.start_conversion(false);

    assert_not_contains(&conversion_state_texts(&engine), "キョウハイイテンキ");
}

#[test]
fn pending_sentence_conversion_keeps_reading_on_repeated_space() {
    let mut engine = composing_engine("きょうはいいてんき");
    engine.start_conversion(false);

    let result = engine.process_key(&press_key(Keysym::SPACE));

    assert_eq!(engine.input_buf.text, "きょうはいいてんき");
    assert!(matches!(engine.state(), InputState::Conversion { .. }));
    assert!(engine.pending_async_request.is_some());
    assert!(result.actions.iter().any(|action| {
        matches!(action, EngineAction::UpdatePreedit(preedit) if preedit.text() == "きょうはいいてんき")
    }));
}

#[test]
fn typing_during_pending_sentence_conversion_resumes_composing_without_data_loss() {
    let mut engine = composing_engine("きょうはいいてんき");
    engine.start_conversion(false);

    let first = engine.process_key(&press('k'));
    let second = engine.process_key(&press('a'));

    assert_eq!(engine.input_buf.text, "きょうはいいてんきか");
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert!(
        !first
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(_)))
    );
    assert!(
        !second
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(_)))
    );
}

#[test]
fn host_commit_during_pending_sentence_conversion_preserves_raw_reading() {
    let mut engine = composing_engine("きょうはいいてんき");
    engine.start_conversion(false);

    let committed = engine.commit();

    assert_eq!(committed, "きょうはいいてんき");
    assert!(matches!(engine.state(), InputState::Empty));
}

#[test]
fn long_dictionary_word_hides_script_variants_without_a_semantic_page() {
    let mut engine = composing_engine("とうきょうと");
    engine.dicts.user = Some(user_dict_with("とうきょうと", "東京都"));
    let texts: Vec<String> = engine
        .build_conversion_candidates("とうきょうと", 9, false)
        .into_iter()
        .map(|candidate| candidate.text)
        .collect();

    assert_contains(&texts, "東京都");
    assert_not_contains(&texts, "とうきょうと");
    assert_not_contains(&texts, "トウキョウト");
    assert_not_contains(&texts, "ﾄｳｷｮｳﾄ");
}

#[test]
fn long_non_hiragana_literal_is_not_filtered_as_a_sentence() {
    assert_contains(&conversion_texts("12345678"), "12345678");
}

#[test]
fn plain_hiragana_word_is_not_wrapped_in_brackets() {
    let texts = conversion_texts("あいう");
    assert_not_contains(&texts, "「あいう」");
    assert_not_contains(&texts, "【あいう】");
}

// ---------- symbol variants ----------

#[test]
fn three_full_stops_emit_ellipsis_without_kanji_model() {
    // Regression: previously `build_conversion_candidates` early-returned
    // when init_kanji_converter failed, so symbol-only inputs lost rewriter
    // variants and `。。。` produced no `…`.
    assert_contains(&conversion_texts("。。。"), "…");
}

// ---------- rewriter scope (the headline regression) ----------

#[test]
fn rewriter_does_not_expand_dictionary_candidates() {
    // The rewriter must operate ONLY on what the user typed, not on
    // dictionary/model/fallback candidates derived from it.
    //
    // Setup: a user dict entry maps `てすと` → `,` (ASCII comma). The
    // SymbolRewriter has `,` → [`、`, `，`, `､`]. If the rewriter were
    // (wrongly) fed dictionary candidates, those three variants would
    // appear even though the user typed only hiragana.
    let mut engine = composing_engine("てすと");
    engine.dicts.user = Some(user_dict_with("てすと", ","));

    let texts: Vec<String> = engine
        .build_conversion_candidates("てすと", 9, false)
        .into_iter()
        .map(|c| c.text)
        .collect();

    assert_contains(&texts, ","); // sanity: dict entry survives
    for forbidden in ["、", "，", "､"] {
        assert_not_contains(&texts, forbidden);
    }
}

#[test]
fn rewriter_candidates_only_derive_from_user_input() {
    // Structural invariant: every Rewriter-source candidate must be a
    // rewrite of the typed reading. Guards against future regressions where
    // somebody re-introduces rewriting over dictionary/model/fallback entries.
    let mut engine = composing_engine("あ");
    let candidates = engine.build_conversion_candidates("あ", 9, false);

    let allowed: HashSet<String> = RewriterChain::default_chain()
        .rewrite_all(&["あ".to_string()])
        .into_iter()
        .map(|(text, _)| text)
        .collect();

    for c in &candidates {
        if c.source == CandidateSource::Rewriter {
            assert!(
                allowed.contains(&c.text),
                "Rewriter candidate `{}` is not a rewrite of input `あ` \
                 (allowed: {:?})",
                c.text,
                allowed
            );
        }
    }
}

// ---------- alphabet width / case variants ----------

#[test]
fn alphabet_input_emits_width_and_case_variants() {
    // Typing `abc` (e.g. in a passthrough or alphabet path) should expand to
    // the other three canonical forms.
    let texts = conversion_texts("abc");
    assert_contains(&texts, "ABC");
    assert_contains(&texts, "ａｂｃ");
    assert_contains(&texts, "ＡＢＣ");
}

#[test]
fn alphabet_variants_carry_width_case_descriptions() {
    let mut engine = composing_engine("abc");
    let candidates = engine.build_conversion_candidates("abc", 9, false);
    let upper = candidates.iter().find(|c| c.text == "ABC").unwrap();
    let full_lower = candidates.iter().find(|c| c.text == "ａｂｃ").unwrap();
    let full_upper = candidates.iter().find(|c| c.text == "ＡＢＣ").unwrap();
    assert_eq!(upper.description.as_deref(), Some("[半]英大文字"));
    assert_eq!(full_lower.description.as_deref(), Some("[全]英小文字"));
    assert_eq!(full_upper.description.as_deref(), Some("[全]英大文字"));
}

// ---------- description (per-candidate right-side comment) ----------

/// Find the candidate with text == `text` in the conversion-state list and
/// return its per-candidate `description` (mozc-style right-side comment).
fn description_for(engine: &InputMethodEngine, text: &str) -> Option<String> {
    engine.state().candidates().and_then(|cl| {
        cl.candidates()
            .iter()
            .find(|c| c.text == text)
            .and_then(|c| c.description.clone())
    })
}

#[test]
fn ellipsis_candidate_carries_three_dot_leader_description() {
    // When `。。。` expands via the rewriter, `…` should carry mozc's
    // description "三点リーダ" as its per-candidate description (not
    // duplicated into the aux text source-label slot).
    let mut engine = InputMethodEngine::new();
    type_string(&mut engine, "..");
    engine.process_key(&press('.'));
    engine.process_key(&press_key(Keysym::SPACE));

    assert_eq!(
        description_for(&engine, "…"),
        Some("三点リーダ".to_string())
    );
}

#[test]
fn typed_symbol_itself_is_annotated_via_global_lookup() {
    // The user typed `「` itself — it appears as a Fallback candidate
    // (not from the rewriter). The post-pass enrichment should still attach
    // mozc's description "始めかぎ括弧" because `「` is a known symbol.
    let mut engine = composing_engine("「");
    let candidates = engine.build_conversion_candidates("「", 9, false);

    let kagi = candidates.iter().find(|c| c.text == "「").unwrap();
    assert_eq!(
        kagi.description.as_deref(),
        Some("始めかぎ括弧"),
        "Fallback `「` should pick up `始めかぎ括弧` via symbol_description"
    );
}

#[test]
fn kagikakko_reading_emits_paired_brackets_in_conversion() {
    // End-to-end: typing the reading `かぎかっこ` and pressing Space should
    // surface `「」` and `『』` in the conversion candidate list (mozc
    // symbol.tsv via SymbolRewriter).
    let texts = conversion_texts("かぎかっこ");
    assert_contains(&texts, "「」");
    assert_contains(&texts, "『』");
}

#[test]
fn katakana_variants_after_a_semantic_page_carry_width_form_description() {
    // mozc-style width annotation: full-width katakana → `[全]カタカナ`,
    // half-width katakana → `[半]カタカナ`. The hiragana fallback also picks
    // up `[全]ひらがな` since hiragana is intrinsically full-width.
    let mut engine = composing_engine("あ");
    engine.dicts.user = Some(user_dict_with_surfaces(
        "あ",
        &[
            "意味00", "意味01", "意味02", "意味03", "意味04", "意味05", "意味06", "意味07",
            "意味08",
        ],
    ));
    let candidates = engine.build_conversion_candidates("あ", 9, false);

    let full = candidates.iter().find(|c| c.text == "ア").unwrap();
    assert_eq!(
        full.description.as_deref(),
        Some("[全]カタカナ"),
        "full-width katakana variant `ア` should be annotated as `[全]カタカナ`",
    );

    let half = candidates.iter().find(|c| c.text == "ｱ").unwrap();
    assert_eq!(
        half.description.as_deref(),
        Some("[半]カタカナ"),
        "half-width katakana variant `ｱ` should be annotated as `[半]カタカナ`",
    );
}

// ---------- end-to-end key flow ----------

#[test]
fn typing_three_dots_emits_ellipsis_in_auto_suggest_and_conversion() {
    // Regression: typing `.` `.` `.` should populate `。。。` and surface `…`
    // both in the auto-suggest list (before Space) and in the conversion
    // candidate list (after Space).
    let mut engine = InputMethodEngine::new();
    type_string(&mut engine, "..");
    let final_result = engine.process_key(&press('.'));
    assert_eq!(engine.input_buf.text, "。。。");

    assert_contains(&auto_suggest_texts(&final_result), "…");

    engine.process_key(&press_key(Keysym::SPACE));
    assert_contains(&conversion_state_texts(&engine), "…");
}

#[test]
fn typing_a_then_space_hides_half_width_katakana_without_a_semantic_page() {
    let mut engine = InputMethodEngine::new();
    type_string(&mut engine, "a");

    let result = engine.process_key(&press_key(Keysym::SPACE));
    assert!(result.consumed);
    assert_not_contains(&conversion_state_texts(&engine), "ｱ");
}

#[test]
fn numeric_counter_homonyms_coexist_and_ignore_model_count() {
    type CandidateFingerprint = (
        usize,
        String,
        CandidateSource,
        Option<String>,
        Option<String>,
    );

    let candidates_for = |num_candidates| {
        let mut engine = composing_engine("10けん");
        engine.dicts.user = Some(user_dict_with("10けん", "十件"));
        engine
            .build_conversion_candidates("10けん", num_candidates, false)
            .into_iter()
            .enumerate()
            .map(|(rank, candidate)| {
                (
                    rank,
                    candidate.text,
                    candidate.source,
                    candidate.reading,
                    candidate.description,
                )
            })
            .collect::<Vec<_>>()
    };

    let candidates_3 = candidates_for(3);
    let candidates_9 = candidates_for(9);
    assert_eq!(candidates_3, candidates_9);

    for expected in ["十件", "10けん", "10ケン", "10ｹﾝ", "10件", "10軒"] {
        assert!(
            candidates_3
                .iter()
                .any(|(_, text, _, _, _)| text == expected),
            "expected deterministic candidate `{expected}`, got {candidates_3:?}"
        );
    }

    let dictionary = candidates_3
        .iter()
        .find(|(_, text, _, _, _)| text == "十件")
        .expect("dictionary candidate");
    assert!(matches!(&dictionary.2, CandidateSource::UserDictionary));

    let counter_ranks: Vec<usize> = ["10件", "10軒"]
        .into_iter()
        .map(|text| {
            let candidate = candidates_3
                .iter()
                .find(|(_, candidate_text, _, _, _)| candidate_text == text)
                .expect("counter rewrite candidate");
            assert!(matches!(&candidate.2, CandidateSource::Rewriter));
            assert!(candidate.0 < 9, "counter candidate must remain on page one");
            candidate.0
        })
        .collect();
    assert!(counter_ranks[0] < counter_ranks[1]);

    let selected_identity = |candidates: &[CandidateFingerprint], rank: usize| {
        let mut list = CandidateList::new(
            candidates
                .iter()
                .map(|(_, text, _, reading, _)| {
                    Candidate::with_reading(text.clone(), reading.clone().unwrap_or_default())
                })
                .collect(),
        );
        list.select(rank);
        (list.cursor(), list.selected().unwrap().text.clone())
    };
    assert_eq!(
        selected_identity(&candidates_3, counter_ranks[0]),
        selected_identity(&candidates_9, counter_ranks[0])
    );
}

#[test]
fn numeric_counter_rewriters_stay_on_first_page_with_many_dictionary_matches() {
    let mut engine = composing_engine("10けん");
    engine.dicts.user = Some(user_dict_with_surfaces(
        "10けん",
        &[
            "辞書00", "辞書01", "辞書02", "辞書03", "辞書04", "辞書05", "辞書06", "辞書07",
            "辞書08", "辞書09", "辞書10", "辞書11",
        ],
    ));

    let candidates = engine.build_conversion_candidates("10けん", 9, false);
    let texts: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.text.as_str())
        .collect();
    let preferred = texts.iter().position(|text| *text == "10件");
    let alternate = texts.iter().position(|text| *text == "10軒");

    assert_eq!(preferred, Some(0));
    assert_eq!(alternate, Some(1));
    assert!(preferred.unwrap() < 9);
    assert!(alternate.unwrap() < 9);
}
