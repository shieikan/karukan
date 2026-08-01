//! Regression tests for the non-blocking conversion worker boundary.

use std::io::Write;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use karukan_engine::Dictionary;

use super::*;
use crate::core::engine::async_conversion::{BackendFactory, ConversionBackend};

fn result_preedit_text(result: &EngineResult) -> Option<String> {
    result.actions.iter().find_map(|action| match action {
        EngineAction::UpdatePreedit(preedit) => Some(preedit.text().to_string()),
        _ => None,
    })
}

struct RecordingBackend {
    calls: Arc<Mutex<Vec<(String, String, usize)>>>,
    name: &'static str,
}

struct MappedBackend {
    mappings: Vec<(String, String)>,
    calls: Arc<Mutex<Vec<String>>>,
}

impl ConversionBackend for MappedBackend {
    fn convert(&mut self, reading: &str, _context: &str, _num_candidates: usize) -> Vec<String> {
        self.calls.lock().unwrap().push(reading.to_string());
        let surface = self
            .mappings
            .iter()
            .find(|(mapped_reading, _)| mapped_reading == reading)
            .map(|(_, surface)| surface.clone())
            .unwrap_or_else(|| format!("model:{reading}"));
        vec![surface]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "mapped-tail-backend"
    }
}

impl ConversionBackend for RecordingBackend {
    fn convert(&mut self, reading: &str, context: &str, num_candidates: usize) -> Vec<String> {
        self.calls.lock().unwrap().push((
            "convert".to_string(),
            format!("{reading}|{context}"),
            num_candidates,
        ));
        vec![format!("model:{reading}")]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        self.calls
            .lock()
            .unwrap()
            .push(("count".to_string(), reading.to_string(), 0));
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        self.name
    }
}

struct NamedBackend {
    name: String,
    delay: Duration,
    calls: Arc<Mutex<Vec<String>>>,
}

impl ConversionBackend for NamedBackend {
    fn convert(&mut self, reading: &str, _context: &str, _num_candidates: usize) -> Vec<String> {
        self.calls.lock().unwrap().push(self.name.clone());
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        vec![format!("{}:{reading}", self.name)]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

struct ParallelGateBackend {
    name: &'static str,
    started: Arc<AtomicUsize>,
}

impl ConversionBackend for ParallelGateBackend {
    fn convert(&mut self, reading: &str, _context: &str, _num_candidates: usize) -> Vec<String> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.started.load(Ordering::SeqCst) < 2 {
            assert!(
                Instant::now() < deadline,
                "ParallelBeam calls were not concurrent"
            );
            thread::yield_now();
        }
        vec![format!("{}:{reading}", self.name)]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        self.name
    }
}

struct FixedCandidatesBackend {
    candidates: Vec<String>,
}

impl ConversionBackend for FixedCandidatesBackend {
    fn convert(&mut self, reading: &str, _context: &str, num_candidates: usize) -> Vec<String> {
        if num_candidates == 1 && reading == "ケン" && self.candidates.iter().any(|c| c == "10軒")
        {
            // The chunk worker concatenates the unchanged numeric prefix with
            // this kana chunk result, producing the full model proposal 10軒.
            vec!["軒".to_string()]
        } else {
            self.candidates.clone()
        }
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "fixed-candidates"
    }
}

struct DelayedFixedCandidatesBackend {
    candidates: Vec<String>,
    delay: Duration,
    started: Arc<AtomicBool>,
}

impl ConversionBackend for DelayedFixedCandidatesBackend {
    fn convert(&mut self, reading: &str, _context: &str, num_candidates: usize) -> Vec<String> {
        self.started.store(true, Ordering::SeqCst);
        thread::sleep(self.delay);
        if num_candidates == 1 && reading == "ケン" && self.candidates.iter().any(|c| c == "10軒")
        {
            vec!["軒".to_string()]
        } else {
            self.candidates.clone()
        }
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "delayed-fixed-candidates"
    }
}

struct DelayedBackend {
    delay: Duration,
    started: Arc<AtomicBool>,
}

struct SynchronizedGate {
    started_calls: AtomicUsize,
    released_calls: AtomicUsize,
    completed_calls: AtomicUsize,
}

struct SynchronizedDelayedBackend {
    gate: Arc<SynchronizedGate>,
}

struct SynchronizedReleaseGuard {
    gate: Arc<SynchronizedGate>,
}

impl SynchronizedReleaseGuard {
    fn new(gate: Arc<SynchronizedGate>) -> Self {
        Self { gate }
    }
}

impl Drop for SynchronizedReleaseGuard {
    fn drop(&mut self) {
        self.gate.released_calls.store(usize::MAX, Ordering::SeqCst);
    }
}

struct ExplicitRequestGate {
    started_calls: AtomicUsize,
    explicit_started: AtomicUsize,
    explicit_completed: AtomicUsize,
    release_through: AtomicUsize,
}

struct GatedRequestBackend {
    gate: Arc<ExplicitRequestGate>,
}

struct ExplicitRequestReleaseGuard {
    gate: Arc<ExplicitRequestGate>,
}

impl ExplicitRequestReleaseGuard {
    fn new(gate: Arc<ExplicitRequestGate>) -> Self {
        Self { gate }
    }
}

impl Drop for ExplicitRequestReleaseGuard {
    fn drop(&mut self) {
        self.gate
            .release_through
            .store(usize::MAX, Ordering::SeqCst);
    }
}

impl Drop for GatedRequestBackend {
    fn drop(&mut self) {
        self.gate
            .release_through
            .store(usize::MAX, Ordering::SeqCst);
    }
}

impl ConversionBackend for GatedRequestBackend {
    fn convert(&mut self, reading: &str, _context: &str, num_candidates: usize) -> Vec<String> {
        let call_number = self.gate.started_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if num_candidates > 1 {
            self.gate.explicit_started.fetch_add(1, Ordering::SeqCst);
        }
        while self.gate.release_through.load(Ordering::SeqCst) < call_number {
            thread::yield_now();
        }
        if num_candidates > 1 {
            self.gate.explicit_completed.fetch_add(1, Ordering::SeqCst);
        }
        vec![format!("gated:{reading}")]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "gated-request-backend"
    }
}

#[test]
fn explicit_request_release_guard_releases_gate_on_unwind() {
    let gate = Arc::new(ExplicitRequestGate {
        started_calls: AtomicUsize::new(0),
        explicit_started: AtomicUsize::new(0),
        explicit_completed: AtomicUsize::new(0),
        release_through: AtomicUsize::new(0),
    });

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
        let gate = Arc::clone(&gate);
        move || {
            let _release_guard = ExplicitRequestReleaseGuard::new(gate);
            panic!("simulate an assertion failure while the gate is held");
        }
    }));

    assert!(result.is_err());
    assert_eq!(
        gate.release_through.load(Ordering::SeqCst),
        usize::MAX,
        "the unwind guard must release the backend before engine teardown"
    );
}

impl ConversionBackend for SynchronizedDelayedBackend {
    fn convert(&mut self, reading: &str, _context: &str, _num_candidates: usize) -> Vec<String> {
        let call_number = self.gate.started_calls.fetch_add(1, Ordering::SeqCst) + 1;
        while self.gate.released_calls.load(Ordering::SeqCst) < call_number {
            thread::yield_now();
        }
        self.gate.completed_calls.fetch_add(1, Ordering::SeqCst);
        vec![format!("converted:{reading}")]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "synchronized-test-backend"
    }
}

impl ConversionBackend for DelayedBackend {
    fn convert(&mut self, reading: &str, _context: &str, _num_candidates: usize) -> Vec<String> {
        self.started.store(true, Ordering::SeqCst);
        thread::sleep(self.delay);
        vec![format!("converted:{reading}")]
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        Ok(reading.chars().count())
    }

    fn model_name(&self) -> &str {
        "test-backend"
    }
}

fn delayed_init_factory(delay: Duration) -> BackendFactory {
    Box::new(move |_variant, _threads| {
        thread::sleep(delay);
        Ok(Box::new(DelayedBackend {
            delay: Duration::ZERO,
            started: Arc::new(AtomicBool::new(false)),
        }) as Box<dyn ConversionBackend>)
    })
}

fn wait_until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(predicate(), "timed out waiting for worker state");
}

fn worker_request(generation: u64) -> crate::core::engine::async_conversion::ConversionRequest {
    crate::core::engine::async_conversion::ConversionRequest {
        snapshot: crate::core::engine::async_conversion::ConversionSnapshot {
            generation,
            reading: "あ".to_string(),
            context: String::new(),
            mode: InputMode::Hiragana,
            input_state: crate::core::engine::async_conversion::AsyncInputState::Composing,
            request_kind: crate::core::engine::async_conversion::AsyncRequestKind::AutoSuggest,
            candidate_count: 1,
            skip_learning: false,
            chunk_cache_epoch: generation,
        },
        chunks: vec![crate::core::engine::async_conversion::ChunkRequest {
            position: 0,
            reading: "あ".to_string(),
            context: String::new(),
            converted: "あ".to_string(),
            should_convert: true,
        }],
        config: EngineConfig::default(),
    }
}

fn init_request_named(main_variant: &str) -> crate::core::engine::async_conversion::InitRequest {
    crate::core::engine::async_conversion::InitRequest {
        main_variant: main_variant.to_string(),
        light_variant: None,
        strategy: crate::config::settings::StrategyMode::Main,
        n_threads: 0,
        config: EngineConfig::default(),
    }
}

fn init_request() -> crate::core::engine::async_conversion::InitRequest {
    init_request_named("main")
}

#[test]
fn slow_backend_never_blocks_key_handler_p95() {
    let started = Arc::new(AtomicBool::new(false));
    let mut engine = InputMethodEngine::with_test_backend(Box::new(DelayedBackend {
        delay: Duration::from_secs(1),
        started: Arc::clone(&started),
    }));
    let mut durations = Vec::with_capacity(20);
    let loop_start = Instant::now();

    for ch in "abcdefghijklmnopqrst".chars() {
        let start = Instant::now();
        engine.process_key(&press(ch));
        durations.push(start.elapsed());
    }

    wait_until(|| started.load(Ordering::SeqCst));
    durations.sort_unstable();
    let p95 = durations[18];
    println!("handler durations: {durations:?}");
    println!("p95: {p95:?}, loop: {:?}", loop_start.elapsed());
    assert!(p95 < Duration::from_millis(100), "p95={p95:?}");
    assert!(loop_start.elapsed() < Duration::from_millis(500));
}

#[test]
fn conversion_mailbox_is_capacity_one_latest_wins() {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend(
        Box::new(SynchronizedDelayedBackend {
            gate: Arc::clone(&gate),
        }),
    );
    worker.submit(worker_request(0));
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    for generation in 1..=4 {
        worker.submit(worker_request(generation));
    }
    assert_eq!(worker.pending_conversion_count(), 1);
    assert_eq!(worker.pending_conversion_generation(), Some(4));

    gate.released_calls.store(1, Ordering::SeqCst);
    wait_until(|| gate.completed_calls.load(Ordering::SeqCst) == 1);
    thread::sleep(Duration::from_millis(5));
    assert_eq!(
        worker
            .poll()
            .map(|completion| completion.snapshot.generation),
        None,
        "an in-flight old completion must not occupy the mailbox ahead of newer pending work"
    );

    gate.released_calls.store(usize::MAX, Ordering::SeqCst);
    wait_until(|| gate.completed_calls.load(Ordering::SeqCst) == 2);
    let deadline = Instant::now() + Duration::from_secs(3);
    let completion = loop {
        if let Some(completion) = worker.poll() {
            break completion;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for latest completion"
        );
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(completion.snapshot.generation, 4);
}

#[test]
fn mutating_key_discards_ready_completion_before_appending_input() {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(SynchronizedDelayedBackend {
        gate: Arc::clone(&gate),
    }));
    let _release_guard = SynchronizedReleaseGuard::new(Arc::clone(&gate));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    let expected_generation = engine.generation;
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    gate.released_calls.store(1, Ordering::SeqCst);
    wait_until(|| {
        gate.completed_calls.load(Ordering::SeqCst) == 1
            && engine.published_completion_generation() == Some(expected_generation)
    });

    // The latest completion is ready before this key mutates the input. The
    // mutating key must run first and invalidate the stale proposal rather
    // than applying a surface for the old reading.
    let result = engine.process_key(&press('o'));
    assert_eq!(engine.input_buf.text, "あいうお");
    assert!(
        !result.actions.iter().any(|action| {
            matches!(
                action,
                EngineAction::UpdatePreedit(preedit)
                    if preedit.text() == "converted:アイウ"
            )
        }),
        "stale completion was applied before the mutating key: {:?}",
        result.actions
    );
}

#[test]
fn ready_async_completion_does_not_consume_a_pass_through_key() {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(SynchronizedDelayedBackend {
        gate: Arc::clone(&gate),
    }));
    let _release_guard = SynchronizedReleaseGuard::new(Arc::clone(&gate));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    let expected_generation = engine.generation;
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    gate.released_calls.store(1, Ordering::SeqCst);
    wait_until(|| {
        gate.completed_calls.load(Ordering::SeqCst) == 1
            && engine.published_completion_generation() == Some(expected_generation)
    });

    let result = engine.process_key(&press_key(Keysym(0xffbe)));
    assert!(!result.consumed, "async UI actions must not swallow F1");
    assert!(result.actions.iter().any(|action| {
        matches!(
            action,
            EngineAction::UpdatePreedit(preedit) if preedit.text() == "converted:アイウ"
        )
    }));
}

fn assert_ready_completion_published_on_state_neutral_boundary(key: KeyEvent) {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(SynchronizedDelayedBackend {
        gate: Arc::clone(&gate),
    }));
    let _release_guard = SynchronizedReleaseGuard::new(Arc::clone(&gate));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    let expected_generation = engine.generation;
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    gate.released_calls.store(1, Ordering::SeqCst);
    wait_until(|| {
        gate.completed_calls.load(Ordering::SeqCst) == 1
            && engine.published_completion_generation() == Some(expected_generation)
    });

    let result = engine.process_key(&key);
    assert!(!result.consumed);
    assert!(result.actions.iter().any(|action| {
        matches!(
            action,
            EngineAction::UpdatePreedit(preedit) if preedit.text() == "converted:アイウ"
        )
    }));
}

#[test]
fn ready_completion_publishes_at_modifier_and_release_boundaries() {
    assert_ready_completion_published_on_state_neutral_boundary(press_key(Keysym::SHIFT_L));
    assert_ready_completion_published_on_state_neutral_boundary(release_key(Keysym::KEY_A));
}

fn assert_live_tail_reopens_to_full_request(
    prefix: &str,
    suffix: &str,
    partial_surface: &str,
    full_surface: &str,
) {
    let full_reading = format!("{prefix}{suffix}");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut engine = InputMethodEngine::with_test_backend(Box::new(MappedBackend {
        mappings: vec![
            (
                karukan_engine::hiragana_to_katakana(prefix),
                partial_surface.to_string(),
            ),
            (
                karukan_engine::hiragana_to_katakana(&full_reading),
                full_surface.to_string(),
            ),
        ],
        calls: Arc::clone(&calls),
    }));
    engine.config.composing_chunk_len = 30;
    engine.input_buf.insert(prefix);
    engine.chunks = vec![ComposingChunk {
        position: 0,
        reading: prefix.to_string(),
        converted: partial_surface.to_string(),
        fresh: true,
    }];
    engine.live.text = partial_surface.to_string();
    engine.set_composing_state();

    engine.input_buf.insert(suffix);
    let projected = engine.refresh_input_state();
    let projected_text = format!("{partial_surface}{suffix}");
    assert_eq!(engine.live.text, projected_text);
    assert_eq!(
        result_preedit_text(&projected).as_deref(),
        Some(projected_text.as_str())
    );

    let completion = wait_for_engine_completion(&mut engine);
    let normalized_full = karukan_engine::hiragana_to_katakana(&full_reading);
    assert!(
        calls.lock().unwrap().contains(&normalized_full),
        "tail append did not request the full reading: {:?}",
        calls.lock().unwrap()
    );
    assert_eq!(engine.live.text, full_surface);
    assert_eq!(engine.chunks[0].reading, full_reading);
    assert_eq!(engine.chunks[0].converted, full_surface);
    assert!(completion.actions.iter().any(|action| {
        matches!(
            action,
            EngineAction::UpdatePreedit(preedit) if preedit.text() == full_surface
        )
    }));
}

#[test]
fn live_tail_reopens_structurally_for_partial_and_full_readings() {
    for (prefix, suffix, partial, full) in [
        ("こう", "いう", "誤", "正"),
        ("うち", "こんだり", "内", "打ち込んだり"),
        ("おしえ", "て", "教", "教えて"),
    ] {
        assert_live_tail_reopens_to_full_request(prefix, suffix, partial, full);
    }
}

fn candidate_texts(engine: &InputMethodEngine) -> Vec<String> {
    engine
        .candidates()
        .map(|candidates| {
            candidates
                .candidates()
                .iter()
                .map(|candidate| candidate.text.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn type_counter(engine: &mut InputMethodEngine) {
    for ch in "10kenn".chars() {
        engine.process_key(&press(ch));
    }
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

type CandidateFingerprint = (String, Option<String>, Option<String>, Option<String>);

fn candidate_fingerprints(engine: &InputMethodEngine) -> Vec<CandidateFingerprint> {
    engine
        .candidates()
        .map(|candidates| {
            candidates
                .candidates()
                .iter()
                .map(|candidate| {
                    (
                        candidate.text.clone(),
                        candidate.source.map(|source| source.label().to_string()),
                        candidate.reading.clone(),
                        candidate.description.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn assert_delayed_composing_auto_suggest_is_stale(
    prepare: impl FnOnce(&mut InputMethodEngine),
    action: impl FnOnce(&mut InputMethodEngine),
) {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(SynchronizedDelayedBackend {
        gate: Arc::clone(&gate),
    }));

    // Drive the real composing path. The third kana creates the delayed
    // auto-suggest request for the current hiragana reading.
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    prepare(&mut engine);

    let generation_before_action = engine.generation;
    action(&mut engine);
    assert!(engine.generation > generation_before_action);

    let expected_generation = engine.generation;
    let expected_input = engine.input_buf.text.clone();
    let expected_caret = engine.input_buf.cursor_pos;
    let expected_mode = engine.mode.current();
    let expected_live = engine.live.text.clone();
    let expected_candidates = candidate_texts(&engine);

    // Release only the request already in flight.  Any request submitted by
    // the mutation remains blocked so this poll cannot accidentally consume a
    // newer completion before checking that the old one was rejected.
    gate.released_calls.store(1, Ordering::SeqCst);
    wait_until(|| gate.completed_calls.load(Ordering::SeqCst) >= 1);
    thread::sleep(Duration::from_millis(5));
    assert!(
        engine.poll_async_conversion().is_none(),
        "stale delayed auto-suggest poll must produce no actions"
    );
    assert_eq!(engine.generation, expected_generation);
    assert_eq!(engine.input_buf.text, expected_input);
    assert_eq!(engine.input_buf.cursor_pos, expected_caret);
    assert_eq!(engine.mode.current(), expected_mode);
    assert_eq!(engine.live.text, expected_live);
    assert_eq!(candidate_texts(&engine), expected_candidates);
    gate.released_calls.store(usize::MAX, Ordering::SeqCst);
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_caret_move() {
    assert_delayed_composing_auto_suggest_is_stale(
        |_| {},
        |engine| {
            engine.process_key(&press_key(Keysym::LEFT));
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_backspace() {
    assert_delayed_composing_auto_suggest_is_stale(
        |_| {},
        |engine| {
            engine.process_key(&press_key(Keysym::BACKSPACE));
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_delete() {
    assert_delayed_composing_auto_suggest_is_stale(
        |engine| {
            engine.process_key(&press('u'));
            engine.process_key(&press_key(Keysym::LEFT));
        },
        |engine| {
            engine.process_key(&press_key(Keysym::DELETE));
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_ctrl_k_katakana() {
    assert_delayed_composing_auto_suggest_is_stale(
        |_| {},
        |engine| {
            engine.process_key(&press_ctrl(Keysym::KEY_K));
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_ctrl_shift_l_live_toggle() {
    assert_delayed_composing_auto_suggest_is_stale(
        |_| {},
        |engine| {
            engine.process_key(&press_ctrl_shift(Keysym::KEY_L));
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_completion_rejected_after_surrounding_context_change() {
    assert_delayed_composing_auto_suggest_is_stale(
        |_| {},
        |engine| {
            engine.set_surrounding_context("新しい左文脈", "新しい右文脈");
        },
    );
}

#[test]
fn delayed_composing_auto_suggest_stale_after_right_super_hiragana_toggle() {
    let gate = Arc::new(SynchronizedGate {
        started_calls: AtomicUsize::new(0),
        released_calls: AtomicUsize::new(0),
        completed_calls: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(SynchronizedDelayedBackend {
        gate: Arc::clone(&gate),
    }));
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);

    // Reproduce the pre-existing Katakana state before the one-way mode key.
    // The pending request still describes the old Hiragana state.
    engine.mode.set(InputMode::Katakana);
    let generation_before_toggle = engine.generation;
    engine.process_key(&press_key(Keysym::SUPER_R));
    assert_eq!(engine.mode.current(), InputMode::Hiragana);
    assert!(engine.generation > generation_before_toggle);

    gate.released_calls.store(usize::MAX, Ordering::SeqCst);
    wait_until(|| gate.completed_calls.load(Ordering::SeqCst) == 1);
    thread::sleep(Duration::from_millis(5));
    assert!(engine.poll_async_conversion().is_none());
}

fn assert_stale_completion_rejected_after(action: impl FnOnce(&mut InputMethodEngine)) {
    let started = Arc::new(AtomicBool::new(false));
    let mut engine = InputMethodEngine::with_test_backend(Box::new(DelayedBackend {
        delay: Duration::from_millis(80),
        started: Arc::clone(&started),
    }));
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press_key(Keysym::SPACE));
    wait_until(|| started.load(Ordering::SeqCst));
    let generation = engine.generation;
    action(&mut engine);
    assert_ne!(engine.generation, generation);
    thread::sleep(Duration::from_millis(120));
    assert!(
        engine.poll_async_conversion().is_none(),
        "stale completion was accepted after invalidation"
    );
}

#[test]
fn stale_completion_rejected_after_navigation_digit_enter_cancel_and_reset() {
    assert_stale_completion_rejected_after(|engine| {
        engine.process_key(&press_key(Keysym::DOWN));
    });
    assert_stale_completion_rejected_after(|engine| {
        engine.process_key(&press('1'));
    });
    assert_stale_completion_rejected_after(|engine| {
        engine.process_key(&press_key(Keysym::RETURN));
    });
    assert_stale_completion_rejected_after(|engine| {
        engine.process_key(&press_key(Keysym::ESCAPE));
    });
    assert_stale_completion_rejected_after(|engine| {
        engine.reset();
    });
}

fn assert_explicit_completion_is_stale_after(
    action: impl FnOnce(&mut InputMethodEngine),
    completion_before_action: bool,
) {
    let gate = Arc::new(ExplicitRequestGate {
        started_calls: AtomicUsize::new(0),
        explicit_started: AtomicUsize::new(0),
        explicit_completed: AtomicUsize::new(0),
        // The auto-suggest call is released so the explicit request can be
        // proven as the second backend call. Every later call stays gated.
        release_through: AtomicUsize::new(1),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(GatedRequestBackend {
        gate: Arc::clone(&gate),
    }));
    // Declare the guard after the engine so unwind drops it before the engine
    // joins the worker that may be blocked in the backend gate.
    let _release_guard = ExplicitRequestReleaseGuard::new(Arc::clone(&gate));
    // Light strategy keeps the request-kind distinction observable at the
    // backend seam: auto-suggest asks for one candidate, explicit conversion
    // asks for a beam. The gate therefore cannot mistake an earlier auto call
    // for the explicit request under test.
    engine.config.strategy = crate::config::settings::StrategyMode::Light;

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    wait_for_explicit_gate(
        &gate,
        || gate.started_calls.load(Ordering::SeqCst) == 1,
        "auto request",
    );
    engine.process_key(&press_key(Keysym::SPACE));
    wait_for_explicit_gate(
        &gate,
        || gate.explicit_started.load(Ordering::SeqCst) == 1,
        "explicit request started",
    );

    if completion_before_action {
        gate.release_through.store(3, Ordering::SeqCst);
        wait_for_explicit_gate(
            &gate,
            || gate.explicit_completed.load(Ordering::SeqCst) == 1,
            "explicit request completed before action",
        );
    }

    action(&mut engine);

    if !completion_before_action {
        gate.release_through.store(3, Ordering::SeqCst);
        wait_for_explicit_gate(
            &gate,
            || gate.explicit_completed.load(Ordering::SeqCst) == 1,
            "explicit request completed after action",
        );
    }

    thread::sleep(Duration::from_millis(5));
    assert!(
        engine.poll_async_conversion().is_none(),
        "stale explicit completion was accepted after the named operation"
    );
}

fn wait_for_explicit_gate(gate: &ExplicitRequestGate, predicate: impl Fn() -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        predicate(),
        "timed out waiting for {label}: started={}, explicit_started={}, explicit_completed={}, release_through={}",
        gate.started_calls.load(Ordering::SeqCst),
        gate.explicit_started.load(Ordering::SeqCst),
        gate.explicit_completed.load(Ordering::SeqCst),
        gate.release_through.load(Ordering::SeqCst)
    );
    assert!(
        gate.started_calls.load(Ordering::SeqCst) < 10,
        "unexpectedly many gated requests while waiting for {label}"
    );
}

#[test]
fn explicit_completion_stale_after_all_named_operations_in_both_orders() {
    for completion_before_action in [false, true] {
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.process_key(&press_key(Keysym::DOWN));
            },
            completion_before_action,
        );
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.process_key(&press('1'));
            },
            completion_before_action,
        );
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.process_key(&press_key(Keysym::RETURN));
            },
            completion_before_action,
        );
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.process_key(&press_key(Keysym::ESCAPE));
            },
            completion_before_action,
        );
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.reset();
            },
            completion_before_action,
        );
        assert_explicit_completion_is_stale_after(
            |engine| {
                engine.process_key(&press('u'));
            },
            completion_before_action,
        );
    }
}

#[test]
fn stale_completion_cannot_mutate_new_generation() {
    let started = Arc::new(AtomicBool::new(false));
    let mut engine = InputMethodEngine::with_test_backend(Box::new(DelayedBackend {
        delay: Duration::from_millis(80),
        started: Arc::clone(&started),
    }));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    wait_until(|| started.load(Ordering::SeqCst));
    engine.process_key(&press('e'));

    thread::sleep(Duration::from_millis(120));
    let _ = engine.poll_async_conversion();
    assert_eq!(engine.input_buf.text, "あいうえ");
    assert!(
        !engine.live.text.contains("あ"),
        "stale completion mutated new input"
    );
}

#[test]
fn slow_initialization_never_runs_on_handler_path() {
    let mut engine =
        InputMethodEngine::with_test_backend_factory(delayed_init_factory(Duration::from_secs(1)));
    engine.begin_test_init();

    let mut durations = Vec::with_capacity(20);
    for ch in "abcdefghijklmnopqrst".chars() {
        let start = Instant::now();
        engine.process_key(&press(ch));
        durations.push(start.elapsed());
    }
    durations.sort_unstable();
    let p95 = durations[18];
    println!("initialization handler durations: {durations:?}");
    println!("initialization p95: {p95:?}");
    assert!(p95 < Duration::from_millis(100));
}

fn wait_for_engine_completion(engine: &mut InputMethodEngine) -> EngineResult {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(result) = engine.poll_async_conversion() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for engine completion"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn core_host_loop_polls_delayed_auto_and_explicit_without_new_key() {
    let mut engine = InputMethodEngine::with_test_backend(Box::new(DelayedBackend {
        delay: Duration::from_millis(20),
        started: Arc::new(AtomicBool::new(false)),
    }));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    assert!(engine.is_ready());
    assert!(engine.has_pending_async_conversion());
    let auto_result = wait_for_engine_completion(&mut engine);
    assert!(!auto_result.actions.is_empty());

    engine.process_key(&press_key(Keysym::SPACE));
    assert!(engine.has_pending_async_conversion());
    let explicit_result = wait_for_engine_completion(&mut engine);
    assert!(
        explicit_result.actions.iter().any(|action| {
            matches!(
                action,
                EngineAction::ShowCandidates(_) | EngineAction::UpdatePreedit(_)
            )
        }),
        "public poll did not return explicit UI actions"
    );
    assert!(!engine.has_pending_async_conversion());
}

#[test]
fn auto_conversion_starts_at_three_kana_and_space_stays_explicit_at_two() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut engine = InputMethodEngine::with_test_backend(Box::new(RecordingBackend {
        calls: Arc::clone(&calls),
        name: "threshold-backend",
    }));

    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    assert!(!engine.has_pending_async_conversion());
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(kind, _, _)| kind != "convert"),
        "one/two kana must not start auto conversion"
    );

    engine.process_key(&press('u'));
    let _ = wait_for_engine_completion(&mut engine);
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| kind == "convert"),
        "the third kana must start auto conversion"
    );

    let explicit_calls = Arc::new(Mutex::new(Vec::new()));
    let mut short_engine = InputMethodEngine::with_test_backend(Box::new(RecordingBackend {
        calls: Arc::clone(&explicit_calls),
        name: "short-explicit-backend",
    }));
    short_engine.process_key(&press('a'));
    short_engine.process_key(&press('i'));
    assert!(!short_engine.has_pending_async_conversion());
    let result = short_engine.process_key(&press_key(Keysym::SPACE));
    assert!(result.consumed);
    assert!(short_engine.has_pending_async_conversion());
    let _ = wait_for_engine_completion(&mut short_engine);
    assert!(
        explicit_calls
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| kind == "convert")
    );
}

#[test]
fn delayed_initialization_keeps_conversion_pending_until_init_succeeds() {
    let mut engine = InputMethodEngine::with_test_backend_factory(delayed_init_factory(
        Duration::from_millis(20),
    ));
    engine.begin_test_init();
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.process_key(&press('u'));
    assert!(engine.has_pending_async_conversion());

    wait_until(|| engine.is_ready());
    let result = wait_for_engine_completion(&mut engine);
    assert!(!result.actions.is_empty());
}

#[test]
fn nonblocking_initialization_results_are_bounded() {
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(|_variant, _threads| Err("test initialization failure".to_string())),
    );

    for _ in 0..32 {
        worker.begin_init(init_request());
    }
    wait_until(|| !worker.is_initializing());

    assert!(
        worker.init_result_count() <= 4,
        "nonblocking init retries must keep bounded outcomes"
    );
}

#[test]
fn hiragana_to_katakana_is_used_for_inference_and_token_count() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend(
        Box::new(RecordingBackend {
            calls: Arc::clone(&calls),
            name: "recording",
        }),
    );
    let mut request = worker_request(1);
    request.snapshot.reading = "ひらがな".to_string();
    request.snapshot.request_kind =
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit;
    request.snapshot.candidate_count = 3;
    worker.submit(request);

    let deadline = Instant::now() + Duration::from_secs(3);
    while worker.poll().is_none() {
        assert!(Instant::now() < deadline, "timed out waiting for inference");
        thread::sleep(Duration::from_millis(2));
    }

    let calls = calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|(kind, reading, _)| kind == "count" && reading == "ヒラガナ"),
        "token counter did not receive normalized katakana: {calls:?}"
    );
    assert!(
        calls.iter().any(|(kind, reading, _)| {
            kind == "convert" && reading.split('|').next() == Some("ヒラガナ")
        }),
        "inference backend did not receive normalized katakana: {calls:?}"
    );
}

#[test]
fn worker_preserves_handler_chunk_positions_duplicates_boundaries_and_context() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend(
        Box::new(RecordingBackend {
            calls: Arc::clone(&calls),
            name: "chunk-recording",
        }),
    );
    let mut request = worker_request(1);
    request.snapshot.request_kind =
        crate::core::engine::async_conversion::AsyncRequestKind::AutoSuggest;
    request.chunks = vec![
        crate::core::engine::async_conversion::ChunkRequest {
            position: 0,
            reading: "あ".to_string(),
            context: "左".to_string(),
            converted: "あ".to_string(),
            should_convert: true,
        },
        crate::core::engine::async_conversion::ChunkRequest {
            position: 1,
            reading: "123,".to_string(),
            context: "左あ".to_string(),
            converted: "123,".to_string(),
            should_convert: false,
        },
        crate::core::engine::async_conversion::ChunkRequest {
            position: 5,
            reading: "あ".to_string(),
            context: "左あ123,".to_string(),
            converted: "あ".to_string(),
            should_convert: true,
        },
    ];
    worker.submit(request);

    let deadline = Instant::now() + Duration::from_secs(3);
    let completion = loop {
        if let Some(completion) = worker.poll() {
            break completion;
        }
        assert!(Instant::now() < deadline, "timed out waiting for chunks");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(
        completion
            .chunk_cache
            .iter()
            .map(|chunk| (chunk.position, chunk.reading.clone()))
            .collect::<Vec<_>>(),
        vec![
            (0, "あ".to_string()),
            (1, "123,".to_string()),
            (5, "あ".to_string())
        ]
    );
    assert_eq!(
        completion
            .chunk_cache
            .iter()
            .map(|chunk| chunk.converted.clone())
            .collect::<Vec<_>>(),
        vec![
            "model:ア".to_string(),
            "123,".to_string(),
            "model:ア".to_string()
        ]
    );
    let calls = calls.lock().unwrap().clone();
    assert!(calls.iter().any(|(_, value, _)| value == "ア|左"));
    assert!(calls.iter().any(|(_, value, _)| value == "ア|左あ123,"));
    assert_eq!(
        calls.iter().filter(|(_, value, _)| value == "123,").count(),
        0
    );
}

#[test]
fn overlapping_init_requests_return_own_tokens_and_publish_only_latest_state() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::clone(&calls);
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(move |variant, _threads| {
            factory_calls.lock().unwrap().push(variant.to_string());
            Ok(Box::new(NamedBackend {
                name: variant.to_string(),
                delay: Duration::from_millis(5),
                calls: Arc::clone(&factory_calls),
            }) as Box<dyn ConversionBackend>)
        }),
    );
    let first = worker.begin_init(init_request());
    let second = worker.begin_init(init_request());

    assert!(
        worker.wait_for_init(first).is_err(),
        "superseded init must not wait for the newer request"
    );
    assert!(worker.wait_for_init(second).is_ok());
    assert_eq!(worker.model_name(), "main");
    assert!(!worker.is_initializing());
}

#[test]
fn in_flight_init_is_superseded_before_it_can_publish_success() {
    let started_calls = Arc::new(AtomicUsize::new(0));
    let release_through = Arc::new(AtomicUsize::new(0));
    let factory_started = Arc::clone(&started_calls);
    let factory_release = Arc::clone(&release_through);
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(move |variant, _threads| {
            let call_number = factory_started.fetch_add(1, Ordering::SeqCst) + 1;
            while factory_release.load(Ordering::SeqCst) < call_number {
                thread::yield_now();
            }
            Ok(Box::new(NamedBackend {
                name: variant.to_string(),
                delay: Duration::ZERO,
                calls: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn ConversionBackend>)
        }),
    );

    let first = worker.begin_init(init_request_named("first"));
    wait_until(|| started_calls.load(Ordering::SeqCst) == 1);
    let second = worker.begin_init(init_request_named("second"));

    assert!(
        worker.wait_for_init(first).is_err(),
        "an in-flight init must publish a superseded outcome for its own token"
    );
    assert!(!worker.is_ready(), "superseded init must not publish ready");
    assert_eq!(worker.model_name(), "unknown");

    release_through.store(1, Ordering::SeqCst);
    wait_until(|| started_calls.load(Ordering::SeqCst) == 2);
    release_through.store(usize::MAX, Ordering::SeqCst);
    assert!(worker.wait_for_init(second).is_ok());
    assert_eq!(worker.model_name(), "second");
    assert!(worker.is_ready());
}

#[test]
fn failed_initialization_can_be_retried_without_publishing_success() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let factory_attempts = Arc::clone(&attempts);
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(move |_variant, _threads| {
            if factory_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("first initialization failed".to_string());
            }
            Ok(Box::new(NamedBackend {
                name: "retried-model".to_string(),
                delay: Duration::ZERO,
                calls: Arc::new(Mutex::new(Vec::new())),
            }) as Box<dyn ConversionBackend>)
        }),
    );

    let first = worker.begin_init(init_request_named("first"));
    assert!(worker.wait_for_init(first).is_err());
    assert!(
        !worker.is_ready(),
        "failed initialization must not publish ready"
    );
    assert!(!worker.is_initializing());

    let retry = worker.begin_init(init_request_named("retry"));
    assert!(worker.wait_for_init(retry).is_ok());
    assert!(worker.is_ready());
    assert_eq!(worker.model_name(), "retried-model");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn parallel_beam_runs_main_and_light_concurrently_with_combined_model_label() {
    let started = Arc::new(AtomicUsize::new(0));
    let factory_started = Arc::clone(&started);
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(move |variant, _threads| {
            let name = if variant == "light" {
                "light-model"
            } else {
                "main-model"
            };
            Ok(Box::new(ParallelGateBackend {
                name,
                started: Arc::clone(&factory_started),
            }) as Box<dyn ConversionBackend>)
        }),
    );
    let mut init = init_request();
    init.light_variant = Some("light".to_string());
    init.strategy = crate::config::settings::StrategyMode::Adaptive;
    let init_token = worker.begin_init(init);
    assert!(worker.wait_for_init(init_token).is_ok());

    let mut request = worker_request(1);
    request.snapshot.request_kind =
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit;
    request.snapshot.candidate_count = 3;
    request.config.strategy = crate::config::settings::StrategyMode::Adaptive;
    worker.submit(request);

    let deadline = Instant::now() + Duration::from_secs(3);
    let completion = loop {
        if let Some(completion) = worker.poll() {
            break completion;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for beam conversion"
        );
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(completion.metrics.model_name, "main-model + light-model");
}

#[test]
fn stale_inference_does_not_commit_adaptive_model_selection() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let factory_calls = Arc::clone(&calls);
    let worker = crate::core::engine::async_conversion::AsyncConversionWorker::with_backend_factory(
        Box::new(move |variant, _threads| {
            Ok(Box::new(NamedBackend {
                name: variant.to_string(),
                delay: if variant == "main" {
                    Duration::from_millis(60)
                } else {
                    Duration::ZERO
                },
                calls: Arc::clone(&factory_calls),
            }) as Box<dyn ConversionBackend>)
        }),
    );
    let mut init = init_request();
    init.light_variant = Some("light".to_string());
    init.strategy = crate::config::settings::StrategyMode::Adaptive;
    let init_token = worker.begin_init(init);
    assert!(worker.wait_for_init(init_token).is_ok());

    let mut old = worker_request(1);
    old.snapshot.request_kind = crate::core::engine::async_conversion::AsyncRequestKind::Explicit;
    old.snapshot.candidate_count = 3;
    old.config.strategy = crate::config::settings::StrategyMode::Adaptive;
    old.config.max_latency_ms = 1;
    worker.submit(old);
    wait_until(|| calls.lock().unwrap().iter().any(|name| name == "main"));

    let mut latest = worker_request(2);
    latest.snapshot.request_kind =
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit;
    latest.snapshot.candidate_count = 3;
    latest.config.strategy = crate::config::settings::StrategyMode::Adaptive;
    latest.config.max_latency_ms = 1;
    worker.submit(latest);

    let deadline = Instant::now() + Duration::from_secs(3);
    let completion = loop {
        if let Some(completion) = worker.poll() {
            break completion;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for latest conversion"
        );
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(completion.snapshot.generation, 2);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().filter(|name| name.as_str() == "main").count(),
        2,
        "stale slow inference incorrectly forced latest work to light model: {calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|name| name.as_str() == "light").count(),
        2,
        "latest ParallelBeam call did not retain both model calls: {calls:?}"
    );
}

fn explicit_completion_engine(model_candidates: Vec<String>) -> InputMethodEngine {
    let mut engine = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
        candidates: model_candidates,
    }));
    let candidates = CandidateList::new(vec![
        Candidate::with_reading("sync-first", "あ"),
        Candidate::with_reading("sync-second", "あ"),
    ]);
    let preedit = Preedit::with_text("sync-first");
    engine.state = InputState::Conversion {
        preedit,
        candidates,
    };
    engine.input_buf.text = "あ".to_string();
    engine.input_buf.cursor_pos = 1;
    engine
}

#[test]
fn explicit_completion_appends_model_candidates_after_sync_candidates() {
    let mut engine =
        explicit_completion_engine(vec!["model-first".to_string(), "model-second".to_string()]);
    engine.submit_async_conversion(
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit,
        false,
    );
    let _ = wait_for_engine_completion(&mut engine);

    assert_eq!(
        candidate_texts(&engine),
        vec!["sync-first", "sync-second", "model-first", "model-second",]
    );
}

#[test]
fn explicit_completion_deduplicates_model_text_without_reordering_sync_candidates() {
    let mut engine =
        explicit_completion_engine(vec!["sync-first".to_string(), "model-only".to_string()]);
    engine.submit_async_conversion(
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit,
        false,
    );
    let _ = wait_for_engine_completion(&mut engine);

    assert_eq!(
        candidate_texts(&engine),
        vec!["sync-first", "sync-second", "model-only"]
    );
}

#[test]
fn explicit_completion_preserves_selected_candidate_identity() {
    let mut engine =
        explicit_completion_engine(vec!["model-first".to_string(), "model-second".to_string()]);
    let _ = engine.state.candidates_mut().unwrap().move_next();
    engine.submit_async_conversion(
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit,
        false,
    );
    let _ = wait_for_engine_completion(&mut engine);

    let candidates = engine.candidates().unwrap();
    assert_eq!(candidates.selected().unwrap().text, "sync-second");
    assert_eq!(candidates.cursor(), 1);
}

#[test]
fn explicit_completion_preserves_live_candidate_as_selected_identity() {
    let gate = Arc::new(ExplicitRequestGate {
        started_calls: AtomicUsize::new(0),
        explicit_started: AtomicUsize::new(0),
        explicit_completed: AtomicUsize::new(0),
        release_through: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(GatedRequestBackend {
        gate: Arc::clone(&gate),
    }));
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    engine.live.text = "愛".to_string();
    engine.process_key(&press_key(Keysym::SPACE));
    gate.release_through.store(usize::MAX, Ordering::SeqCst);

    assert_eq!(
        candidate_texts(&engine).first().map(String::as_str),
        Some("愛")
    );
    assert_eq!(engine.candidates().unwrap().cursor(), 0);
    assert_eq!(engine.candidates().unwrap().selected().unwrap().text, "愛");

    let _ = wait_for_engine_completion(&mut engine);
    let candidates = engine.candidates().unwrap();
    assert_eq!(candidates.candidates().first().unwrap().text, "愛");
    assert_eq!(candidates.cursor(), 0);
    assert_eq!(candidates.selected().unwrap().text, "愛");
}

#[test]
fn delayed_explicit_completion_merges_late_dictionary_candidates_without_retry() {
    let gate = Arc::new(ExplicitRequestGate {
        started_calls: AtomicUsize::new(0),
        explicit_started: AtomicUsize::new(0),
        explicit_completed: AtomicUsize::new(0),
        release_through: AtomicUsize::new(0),
    });
    let mut engine = InputMethodEngine::with_test_backend(Box::new(GatedRequestBackend {
        gate: Arc::clone(&gate),
    }));
    engine.config.num_candidates = 3;
    let asset_deadline = Instant::now() + Duration::from_secs(3);
    while engine.has_pending_async_conversion() {
        engine.poll_init_assets();
        assert!(
            Instant::now() < asset_deadline,
            "timed out draining test assets"
        );
        thread::sleep(Duration::from_millis(2));
    }
    engine.dicts.user = None;
    engine.process_key(&press('a'));
    engine.process_key(&press('i'));
    assert!(engine.dicts.user.is_none());

    let _ = engine.process_key(&press_key(Keysym::SPACE));
    wait_until(|| gate.started_calls.load(Ordering::SeqCst) == 1);
    let before = engine.candidates().expect("Space must enter conversion");
    let selected_before = before.selected().unwrap().clone();
    let cursor_before = before.cursor();
    let preedit_before = match &engine.state {
        InputState::Conversion { preedit, .. } => preedit.text().to_string(),
        _ => panic!("Space must set conversion preedit"),
    };

    engine.poll_init_assets();
    engine.dicts.user = None;
    engine.dicts.user = Some(user_dict_with_surfaces(
        "あい",
        &["late-dict", "gated:アイ"],
    ));
    assert!(
        engine.poll_async_conversion().is_none(),
        "the in-flight model request must still be pending after assets become ready"
    );
    gate.release_through.store(usize::MAX, Ordering::SeqCst);
    let _ = wait_for_engine_completion(&mut engine);

    let after = engine
        .candidates()
        .expect("explicit completion must retain candidates");
    assert_eq!(after.cursor(), cursor_before);
    assert_eq!(after.selected().unwrap().text, selected_before.text);
    assert_eq!(after.selected().unwrap().reading, selected_before.reading);
    let preedit_after = match &engine.state {
        InputState::Conversion { preedit, .. } => preedit.text().to_string(),
        _ => panic!("explicit completion must retain conversion preedit"),
    };
    assert_eq!(preedit_after, preedit_before);
    let texts = candidate_texts(&engine);
    assert!(
        texts.iter().any(|text| text == "late-dict"),
        "late dictionary candidate missing: {texts:?}"
    );
    assert_eq!(
        texts.iter().filter(|text| *text == "gated:アイ").count(),
        1,
        "late synchronous and model candidates must be deduplicated by text"
    );
    assert_eq!(gate.started_calls.load(Ordering::SeqCst), 1);
    assert!(!engine.has_pending_async_conversion());
}

#[test]
fn completion_matching_uses_current_engine_request_metadata() {
    let mut engine = explicit_completion_engine(vec!["model-only".to_string()]);
    engine.submit_async_conversion(
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit,
        false,
    );
    engine.pending_async_request.as_mut().unwrap().skip_learning = true;

    thread::sleep(Duration::from_millis(10));
    assert!(
        engine.poll_async_conversion().is_none(),
        "completion was accepted from stale request metadata"
    );
}

#[test]
fn delayed_model_preserves_counter_selection_and_page_identity() {
    let started = Arc::new(AtomicBool::new(false));
    let mut engine =
        InputMethodEngine::with_test_backend(Box::new(DelayedFixedCandidatesBackend {
            candidates: vec![
                "10件-08".to_string(),
                "10件-09".to_string(),
                "10件-10".to_string(),
            ],
            delay: Duration::from_millis(20),
            started: Arc::clone(&started),
        }));

    let sync_candidates = (0..9)
        .map(|index| {
            let mut candidate = Candidate::with_reading(format!("10件-{index:02}"), "10けん");
            candidate.source = Some(CandidateSource::Rewriter);
            candidate.description = Some("件".to_string());
            candidate
        })
        .collect();
    engine.state = InputState::Conversion {
        preedit: Preedit::with_text("10件-08"),
        candidates: CandidateList::new(sync_candidates),
    };
    engine.input_buf.text = "10けん".to_string();
    engine.input_buf.cursor_pos = "10けん".chars().count();
    for _ in 0..8 {
        let _ = engine.state.candidates_mut().unwrap().move_next();
    }

    engine.submit_async_conversion(
        crate::core::engine::async_conversion::AsyncRequestKind::Explicit,
        false,
    );
    wait_until(|| started.load(Ordering::SeqCst));

    let before = engine.candidates().unwrap();
    assert_eq!(before.page_size(), 9);
    assert_eq!(before.total_pages(), 1);
    assert_eq!(before.cursor(), 8);
    assert_eq!(before.selected().unwrap().text, "10件-08");
    assert_eq!(before.selected().unwrap().source_label(), Some("🔄 変換"));

    let _ = wait_for_engine_completion(&mut engine);
    let after = engine.candidates().unwrap();
    assert_eq!(after.page_size(), 9);
    assert_eq!(after.total_pages(), 2);
    assert_eq!(after.cursor(), 8);
    assert_eq!(after.current_page(), 0);
    assert_eq!(after.selected().unwrap().text, "10件-08");
    assert_eq!(after.selected().unwrap().source_label(), Some("🔄 変換"));

    let page_result = engine.process_key(&press_key(Keysym::PAGE_DOWN));
    let shown = page_result
        .actions
        .iter()
        .find_map(|action| match action {
            EngineAction::ShowCandidates(list) => Some(list),
            _ => None,
        })
        .expect("page navigation must serialize candidates");
    assert_eq!(shown.page_size(), 9);
    assert_eq!(shown.total_pages(), 2);
    assert_eq!(shown.current_page(), 1);
    assert_eq!(shown.page_cursor(), 0);
    assert_eq!(shown.page_candidates()[0].text, "10件-09");
}

#[test]
fn counter_auto_completion_prefers_closed_table_live_text_when_model_returns_homonym_first() {
    for num_candidates in [3, 9] {
        let mut engine = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
            candidates: vec!["10軒".to_string(), "model-only".to_string()],
        }));
        engine.config.num_candidates = num_candidates;
        type_counter(&mut engine);

        let _ = wait_for_engine_completion(&mut engine);
        assert_eq!(engine.live.text, "10件", "num_candidates={num_candidates}");
    }
}

#[test]
fn counter_live_conversion_direct_enter_commits_preferred_surface() {
    for num_candidates in [3, 9] {
        let mut engine = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
            candidates: vec!["10軒".to_string(), "model-only".to_string()],
        }));
        engine.config.num_candidates = num_candidates;
        type_counter(&mut engine);
        let _ = wait_for_engine_completion(&mut engine);

        let result = engine.process_key(&press_key(Keysym::RETURN));
        let committed = result.actions.iter().find_map(|action| match action {
            EngineAction::Commit(text) => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(committed, Some("10件"), "num_candidates={num_candidates}");
    }
}

#[test]
fn counter_live_conversion_explicit_commit_preserves_preferred_surface() {
    for num_candidates in [3, 9] {
        let mut engine = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
            candidates: vec!["10軒".to_string(), "model-only".to_string()],
        }));
        engine.config.num_candidates = num_candidates;
        type_counter(&mut engine);
        let _ = wait_for_engine_completion(&mut engine);

        assert_eq!(engine.commit(), "10件", "num_candidates={num_candidates}");
    }
}

#[test]
fn counter_live_to_space_selects_preferred_surface_for_model_counts_3_and_9() {
    for num_candidates in [3, 9] {
        let mut engine = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
            candidates: vec!["10軒".to_string(), "model-only".to_string()],
        }));
        engine.config.num_candidates = num_candidates;
        type_counter(&mut engine);
        let _ = wait_for_engine_completion(&mut engine);

        let _ = engine.process_key(&press_key(Keysym::SPACE));
        let candidates = engine.candidates().expect("Space must enter conversion");
        assert_eq!(candidates.cursor(), 0, "num_candidates={num_candidates}");
        assert_eq!(candidates.selected().unwrap().text, "10件");
        assert!(
            candidates
                .candidates()
                .iter()
                .any(|candidate| candidate.text == "10軒")
        );

        let _ = wait_for_engine_completion(&mut engine);
        let candidates = engine.candidates().unwrap();
        assert_eq!(
            candidates.cursor(),
            0,
            "selected candidate moved after model merge"
        );
        assert_eq!(candidates.selected().unwrap().text, "10件");
    }
}

#[test]
fn delayed_counter_auto_completion_preserves_candidates_selection_and_page_identity() {
    let started = Arc::new(AtomicBool::new(false));
    let mut engine =
        InputMethodEngine::with_test_backend(Box::new(DelayedFixedCandidatesBackend {
            candidates: vec![
                "10件".to_string(),
                "10軒".to_string(),
                "10軒".to_string(),
                "model-only".to_string(),
            ],
            delay: Duration::from_millis(25),
            started: Arc::clone(&started),
        }));
    engine.dicts.user = Some(user_dict_with_surfaces(
        "10けん",
        &[
            "辞書00", "辞書01", "辞書02", "辞書03", "辞書04", "辞書05", "辞書06", "辞書07",
        ],
    ));

    type_counter(&mut engine);
    wait_until(|| started.load(Ordering::SeqCst));
    let _ = wait_for_engine_completion(&mut engine);
    assert_eq!(engine.live.text, "10件");

    started.store(false, Ordering::SeqCst);
    let _ = engine.process_key(&press_key(Keysym::SPACE));
    let before = engine.candidates().expect("Space must enter conversion");
    assert_eq!(before.page_size(), 9);
    assert_eq!(before.current_page(), 0);
    assert_eq!(before.total_pages(), 2);
    assert_eq!(before.cursor(), 0);
    assert_eq!(before.selected().unwrap().text, "10件");
    let before_fingerprints = candidate_fingerprints(&engine);
    for expected in ["10けん", "10ケン", "10ｹﾝ", "10件", "10軒"] {
        assert!(
            before_fingerprints
                .iter()
                .any(|(text, _, _, _)| text == expected),
            "missing deterministic candidate before delayed merge: {expected}"
        );
    }
    for _ in 0..8 {
        let _ = engine.state.candidates_mut().unwrap().move_next();
    }
    let selected_before = engine.candidates().unwrap().selected().unwrap().clone();
    let cursor_before = engine.candidates().unwrap().cursor();
    let page_before = engine.candidates().unwrap().current_page();

    wait_until(|| started.load(Ordering::SeqCst));
    let _ = wait_for_engine_completion(&mut engine);
    let after = engine
        .candidates()
        .expect("explicit completion must retain candidates");
    assert_eq!(after.page_size(), 9);
    assert_eq!(after.current_page(), page_before);
    assert_eq!(after.cursor(), cursor_before);
    assert_eq!(after.selected().unwrap().text, selected_before.text);
    assert_eq!(after.selected().unwrap().reading, selected_before.reading);
    assert_eq!(after.total_pages(), 2);

    let after_texts = candidate_texts(&engine);
    assert_eq!(after_texts.iter().filter(|text| *text == "10件").count(), 1);
    assert_eq!(after_texts.iter().filter(|text| *text == "10軒").count(), 1);
    assert!(after_texts.iter().any(|text| text == "model-only"));
    for (text, source, reading, description) in before_fingerprints {
        assert!(
            after.candidates().iter().any(|candidate| {
                candidate.text == text
                    && candidate.source.map(|source| source.label().to_string()) == source
                    && candidate.reading == reading
                    && candidate.description == description
            }),
            "deterministic candidate identity changed after delayed merge: {text}"
        );
    }
}

#[test]
fn counter_preference_is_scoped_while_non_counter_remains_model_first() {
    let mut non_counter = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
        candidates: vec!["model-first".to_string(), "model-second".to_string()],
    }));
    for ch in "aiu".chars() {
        non_counter.process_key(&press(ch));
    }
    let _ = wait_for_engine_completion(&mut non_counter);
    assert_eq!(non_counter.live.text, "model-first");
    let result = non_counter.process_key(&press_key(Keysym::RETURN));
    assert!(
        result.actions.iter().any(|action| {
            matches!(action, EngineAction::Commit(text) if text == "model-first")
        })
    );

    let mut counter = InputMethodEngine::with_test_backend(Box::new(FixedCandidatesBackend {
        candidates: vec!["10軒".to_string(), "model-second".to_string()],
    }));
    type_counter(&mut counter);
    let _ = wait_for_engine_completion(&mut counter);
    assert_eq!(counter.live.text, "10件");
}
