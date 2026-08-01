//! Tests for the learning cache and the Tab-skips-learning behavior.
//!
//! Space/Down: include learning candidates (default conversion).
//! Tab: skip learning candidates (lets users escape stale learned entries).

use karukan_engine::LearningCache;

use super::*;

fn engine_with_cache(cache: LearningCache) -> InputMethodEngine {
    let mut engine = InputMethodEngine::new();
    engine.converters.kanji = None;
    engine.learning = Some(cache);
    engine
}

/// Engine seeded with a learning entry `reading → surface`, no kanji model.
/// We bypass `init.rs` (which gates learning on settings + file I/O) and just
/// inject a populated `LearningCache` directly so these tests can exercise
/// candidate behavior without depending on settings or file I/O.
fn engine_with_learned(reading: &str, surface: &str) -> InputMethodEngine {
    let mut cache = LearningCache::new(100);
    cache.record(reading, surface);
    engine_with_cache(cache)
}

fn candidate_texts(
    engine: &mut InputMethodEngine,
    reading: &str,
    skip_learning: bool,
) -> Vec<String> {
    engine
        .build_conversion_candidates(reading, 9, skip_learning)
        .into_iter()
        .map(|candidate| candidate.text)
        .collect()
}

fn cache_from_tsv(contents: &str) -> LearningCache {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), contents).unwrap();
    LearningCache::load(file.path(), 100).unwrap()
}

/// Texts from the most recent ShowCandidates action in the composing phase.
fn auto_suggest_texts(result: &crate::core::engine::EngineResult) -> Vec<String> {
    use crate::core::engine::EngineAction;

    result
        .actions
        .iter()
        .find_map(|action| match action {
            EngineAction::ShowCandidates(list) => Some(
                list.candidates()
                    .iter()
                    .map(|candidate| candidate.text.clone())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn build_candidates_includes_learning_when_not_skipped() {
    let mut engine = engine_with_learned("あい", "藍");

    let texts = candidate_texts(&mut engine, "あい", false);

    assert!(
        texts.contains(&"藍".to_string()),
        "Space path (skip_learning=false) should surface learned `藍`, got {:?}",
        texts,
    );
}

#[test]
fn build_candidates_omits_learning_when_skipped() {
    let mut engine = engine_with_learned("あい", "藍");

    let texts = candidate_texts(&mut engine, "あい", true);

    assert!(
        !texts.contains(&"藍".to_string()),
        "Tab path (skip_learning=true) must drop learned `藍`, got {:?}",
        texts,
    );
}

#[test]
fn normal_auto_suggest_uses_only_the_completed_reading() {
    let mut cache = LearningCache::new(100);
    cache.record("あいう", "長文");
    let mut engine = engine_with_cache(cache);

    let first = engine.process_key(&press('a'));
    assert!(first.consumed);
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert!(
        first
            .actions
            .iter()
            .any(|action| matches!(action, crate::core::engine::EngineAction::UpdatePreedit(_)))
    );

    let short_reading = engine.process_key(&press('i'));
    assert!(short_reading.consumed);
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    assert!(
        short_reading
            .actions
            .iter()
            .any(|action| matches!(action, crate::core::engine::EngineAction::UpdatePreedit(_)))
    );
    assert!(
        short_reading
            .actions
            .iter()
            .any(|action| matches!(action, crate::core::engine::EngineAction::ShowCandidates(_)))
    );
    let short_texts = auto_suggest_texts(&short_reading);

    assert!(
        !short_texts.contains(&"長文".to_string()),
        "normal auto-suggest must exclude a longer learned reading before Space, got {short_texts:?}"
    );

    let completed_reading = engine.process_key(&press('u'));
    assert!(completed_reading.consumed);
    assert!(matches!(engine.state(), InputState::Composing { .. }));
    let completed_texts = auto_suggest_texts(&completed_reading);
    assert!(
        completed_texts.contains(&"長文".to_string()),
        "normal auto-suggest must include the exact learned surface after completion, got {completed_texts:?}"
    );
}

#[test]
fn explicit_space_does_not_suggest_long_learned_phrase_for_short_prefix() {
    let mut cache = LearningCache::new(100);
    cache.record("あいう", "長文");
    let mut engine = engine_with_cache(cache);

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    let result = engine.process_key(&press_key(Keysym::SPACE));

    assert!(result.consumed);
    let texts: Vec<String> = engine
        .state()
        .candidates()
        .unwrap()
        .candidates()
        .iter()
        .map(|candidate| candidate.text.clone())
        .collect();
    assert!(
        !texts.contains(&"長文".to_string()),
        "explicit Space must use exact completed readings, got {texts:?}"
    );
}

#[test]
fn exact_learning_candidates_preserve_frequency_order() {
    let mut cache = LearningCache::new(100);
    cache.record("あい", "高頻度");
    cache.record("あい", "高頻度");
    cache.record("あい", "低頻度");
    let mut engine = engine_with_cache(cache);

    let texts = candidate_texts(&mut engine, "あい", false);

    assert_eq!(&texts[..2], ["高頻度", "低頻度"]);
}

#[test]
fn exact_learning_candidates_preserve_recency_order() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let cache = cache_from_tsv(&format!(
        "# karukan learning cache v1\nあい\t旧\t1\t{}\nあい\t新\t1\t{}\n",
        now.saturating_sub(30 * 86_400),
        now,
    ));
    let mut engine = engine_with_cache(cache);

    let texts = candidate_texts(&mut engine, "あい", false);

    assert_eq!(&texts[..2], ["新", "旧"]);
}

#[test]
fn tab_key_skips_learning_in_composing() {
    // End-to-end: type the reading, press Tab → learned candidate is gone.
    let mut engine = engine_with_learned("あい", "藍");

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    assert_eq!(engine.input_buf.text, "あい");

    let result = engine.process_key(&press_key(Keysym::TAB));
    assert!(result.consumed);
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let texts: Vec<String> = engine
        .state()
        .candidates()
        .unwrap()
        .candidates()
        .iter()
        .map(|c| c.text.clone())
        .collect();
    assert!(
        !texts.contains(&"藍".to_string()),
        "Tab must skip the learned `藍` candidate, got {:?}",
        texts,
    );
}

#[test]
fn space_key_keeps_learning_in_composing() {
    // Counterpart to tab_key_skips_learning_in_composing: Space stays on the
    // learning-included path so the default UX is unchanged.
    let mut engine = engine_with_learned("あい", "藍");

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));

    let result = engine.process_key(&press_key(Keysym::SPACE));
    assert!(result.consumed);
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let texts: Vec<String> = engine
        .state()
        .candidates()
        .unwrap()
        .candidates()
        .iter()
        .map(|c| c.text.clone())
        .collect();
    assert!(
        texts.contains(&"藍".to_string()),
        "Space must surface learned `藍`, got {:?}",
        texts,
    );
}
