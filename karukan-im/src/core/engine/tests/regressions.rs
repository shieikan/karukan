//! Regression coverage for IME editing and mode-transition behavior.

use super::*;

#[test]
fn backspace_after_invalid_romaji_restores_pending_consonant() {
    let mut engine = InputMethodEngine::new();

    // Pressed keys: d, s, Backspace, a. The second `d` in the visible
    // sequence is the restored preedit state, not another keypress.
    engine.process_key(&press('d'));
    assert_eq!(engine.preedit().unwrap().text(), "d");

    engine.process_key(&press('s'));
    assert_eq!(engine.preedit().unwrap().text(), "ds");

    engine.process_key(&press_key(Keysym::BACKSPACE));
    assert_eq!(engine.preedit().unwrap().text(), "d");

    engine.process_key(&press('a'));
    assert_eq!(engine.preedit().unwrap().text(), "だ");
}

#[test]
fn restored_consonant_can_still_form_sokuon() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press('d'));
    engine.process_key(&press('s'));
    engine.process_key(&press_key(Keysym::BACKSPACE));
    engine.process_key(&press('d'));
    engine.process_key(&press('a'));

    assert_eq!(engine.preedit().unwrap().text(), "っだ");
}

#[test]
fn shift_alphabet_commit_returns_next_input_to_hiragana() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press_shift('H'));
    engine.process_key(&press('i'));
    let result = engine.process_key(&press_key(Keysym::RETURN));

    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(text) if text == "Hi"))
    );
    assert_eq!(engine.mode.current(), InputMode::Hiragana);

    engine.process_key(&press('k'));
    engine.process_key(&press('a'));
    assert_eq!(engine.preedit().unwrap().text(), "か");
}

#[test]
fn digit_candidate_commit_restores_temporary_alphabet_mode() {
    let mut engine = InputMethodEngine::new();

    // Frontends use this public page-selection API for number-key commits.
    engine.process_key(&press_shift('A'));
    engine.process_key(&press('b'));
    engine.process_key(&press_key(Keysym::TAB));
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    let result = engine.select_candidate_on_page(0);
    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(_)))
    );
    assert_eq!(engine.mode.current(), InputMode::Hiragana);

    engine.process_key(&press('a'));
    assert_eq!(engine.preedit().unwrap().text(), "あ");
}

#[test]
fn digit_candidate_commit_clears_input_buffer_cursor() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press_shift('A'));
    engine.process_key(&press('b'));
    engine.process_key(&press_key(Keysym::TAB));
    assert!(!engine.input_buf.text.is_empty());
    // Candidate conversion currently places the cursor at zero, but the
    // commit path must preserve InputBuffer's invariant for any valid state.
    engine.input_buf.cursor_pos = engine.input_buf.text.chars().count();

    engine.select_candidate_on_page(0);

    assert!(engine.input_buf.text.is_empty());
    assert_eq!(engine.input_buf.cursor_pos, 0);
}

#[test]
fn public_commit_result_restores_temporary_alphabet_mode() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press_shift('H'));
    engine.process_key(&press('i'));
    let result = engine.commit_result();

    assert!(
        result
            .actions
            .iter()
            .any(|action| matches!(action, EngineAction::Commit(text) if text == "Hi"))
    );
    assert_eq!(engine.mode.current(), InputMode::Hiragana);

    engine.process_key(&press('a'));
    assert_eq!(engine.preedit().unwrap().text(), "あ");
}

#[test]
fn public_commit_restores_temporary_alphabet_mode_from_conversion() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press_shift('A'));
    engine.process_key(&press('b'));
    engine.process_key(&press_key(Keysym::TAB));
    assert!(matches!(engine.state(), InputState::Conversion { .. }));

    assert!(!engine.commit().is_empty());
    assert_eq!(engine.mode.current(), InputMode::Hiragana);
}

#[test]
fn shift_letter_does_not_leave_emoji_mode_from_empty_state() {
    let mut engine = InputMethodEngine::new();
    engine.mode.enter_temporary(InputMode::Emoji);

    engine.process_key(&press_shift('H'));

    assert_eq!(engine.mode.current(), InputMode::Emoji);
    assert_eq!(engine.preedit().unwrap().text(), "h");
}

#[test]
fn shift_letter_does_not_leave_emoji_mode_while_composing() {
    let mut engine = InputMethodEngine::new();
    engine.process_key(&press(':'));
    assert_eq!(engine.mode.current(), InputMode::Emoji);

    engine.process_key(&press_shift('H'));

    assert_eq!(engine.mode.current(), InputMode::Emoji);
    assert_eq!(engine.preedit().unwrap().text(), ":H");
}

#[test]
fn bare_space_in_empty_hiragana_commits_halfwidth_space() {
    let mut engine = InputMethodEngine::new();

    let result = engine.process_key(&press_key(Keysym::SPACE));
    let committed = result.actions.iter().find_map(|action| match action {
        EngineAction::Commit(text) => Some(text.as_str()),
        _ => None,
    });

    assert_eq!(committed, Some(" "));
}

#[test]
fn ctrl_space_uses_halfwidth_space_while_composing() {
    let mut engine = InputMethodEngine::new();

    engine.process_key(&press('a'));
    engine.process_key(&press_ctrl(Keysym::SPACE));

    assert_eq!(engine.preedit().unwrap().text(), "あ ");
}

#[test]
fn ctrl_space_stabilizes_pending_romaji_before_inserting_space() {
    let mut engine = InputMethodEngine::new();

    // The space is unrelated composed text, so pending romaji must be flushed
    // before insertion; later Backspace must not delete the wrong character.
    engine.process_key(&press('d'));
    engine.process_key(&press('s'));
    assert_eq!(engine.preedit().unwrap().text(), "ds");

    engine.process_key(&press_ctrl(Keysym::SPACE));
    assert_eq!(engine.preedit().unwrap().text(), "ds ");

    engine.process_key(&press('x'));
    assert_eq!(engine.preedit().unwrap().text(), "ds x");

    engine.process_key(&press_key(Keysym::BACKSPACE));
    assert_eq!(engine.preedit().unwrap().text(), "ds ");

    engine.process_key(&press_key(Keysym::BACKSPACE));
    assert_eq!(engine.preedit().unwrap().text(), "ds");
}
