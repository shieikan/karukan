//! Single-owner asynchronous conversion worker.
//!
//! The engine thread owns IME state and this worker owns every kanji converter.
//! Requests and completions are exchanged as immutable snapshots.  A bounded
//! latest-wins mailbox keeps typing responsive when inference is slower than
//! the key stream.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::strategy::determine_conversion_strategy;
use super::types::ComposingChunk;
use super::{ConversionStrategy, EngineConfig, InputMode};
use crate::config::settings::StrategyMode;

const MAX_INIT_RESULTS: usize = 4;

/// A dependency-free backend seam used by the worker and by deterministic tests.
pub(crate) trait ConversionBackend: Send {
    fn convert(&mut self, reading: &str, context: &str, num_candidates: usize) -> Vec<String>;
    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String>;
    fn model_name(&self) -> &str;
}

/// Factory used only by the worker thread, so model construction cannot occur
/// on the engine/handler thread.
pub(crate) type BackendFactory =
    Box<dyn Fn(&str, u32) -> Result<Box<dyn ConversionBackend>, String> + Send + 'static>;

struct KanaKanjiBackend {
    converter: karukan_engine::KanaKanjiConverter,
    name: String,
}

impl ConversionBackend for KanaKanjiBackend {
    fn convert(&mut self, reading: &str, context: &str, num_candidates: usize) -> Vec<String> {
        self.converter
            .convert(reading, context, num_candidates)
            .unwrap_or_default()
    }

    fn count_input_tokens(&mut self, reading: &str) -> Result<usize, String> {
        self.converter
            .count_input_tokens(reading)
            .map_err(|error| error.to_string())
    }

    fn model_name(&self) -> &str {
        &self.name
    }
}

fn production_backend_factory() -> BackendFactory {
    Box::new(|variant_id, n_threads| {
        let backend = karukan_engine::Backend::from_variant_id(variant_id)
            .map_err(|error| error.to_string())?;
        let mut converter =
            karukan_engine::KanaKanjiConverter::new(backend).map_err(|error| error.to_string())?;
        if n_threads > 0 {
            converter.set_n_threads(n_threads);
        }
        let name = converter.model_display_name().to_string();
        Ok(Box::new(KanaKanjiBackend { converter, name }) as Box<dyn ConversionBackend>)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsyncRequestKind {
    AutoSuggest,
    Explicit,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingRequestMetadata {
    pub request_kind: AsyncRequestKind,
    pub candidate_count: usize,
    pub skip_learning: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsyncInputState {
    Empty,
    Composing,
    Conversion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversionSnapshot {
    pub generation: u64,
    pub reading: String,
    pub context: String,
    pub mode: InputMode,
    pub input_state: AsyncInputState,
    pub request_kind: AsyncRequestKind,
    pub candidate_count: usize,
    pub skip_learning: bool,
    pub chunk_cache_epoch: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ChunkRequest {
    /// Character offset in the handler-produced chunk plan.
    pub position: usize,
    /// The exact chunk reading selected by the handler.
    pub reading: String,
    /// The exact left context selected by the handler.
    pub context: String,
    /// Current handler-side conversion, used as the immediate fallback/cache.
    pub converted: String,
    /// Whether this chunk is a changed Japanese chunk requiring inference.
    pub should_convert: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ConversionRequest {
    pub snapshot: ConversionSnapshot,
    pub chunks: Vec<ChunkRequest>,
    pub config: EngineConfig,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProposedMetrics {
    pub conversion_ms: u64,
    pub model_name: String,
    pub adaptive_use_light_model: bool,
    pub token_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProposedCompletion {
    pub snapshot: ConversionSnapshot,
    pub model_candidates: Vec<String>,
    pub chunk_cache: Vec<ComposingChunk>,
    pub metrics: ProposedMetrics,
    pub ready: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct InitRequest {
    pub main_variant: String,
    pub light_variant: Option<String>,
    pub strategy: StrategyMode,
    pub n_threads: u32,
    pub config: EngineConfig,
}

#[derive(Debug, Default)]
struct WorkerState {
    pending_init: Option<(u64, InitRequest)>,
    pending_conversion: Option<ConversionRequest>,
    conversion_epoch: u64,
    completion: Option<ProposedCompletion>,
    stopped: bool,
    initializing: bool,
    converting: bool,
    current_init_epoch: u64,
    /// Bounded outcomes for callers using the synchronous init compatibility
    /// API. Nonblocking production callers never consume these tokens.
    init_results: VecDeque<(u64, Result<(), String>)>,
    ready: bool,
    init_error: Option<String>,
    model_name: String,
}

struct SharedWorkerState {
    state: Mutex<WorkerState>,
    wake: Condvar,
}

struct WorkerRuntime {
    factory: BackendFactory,
    main: Option<Box<dyn ConversionBackend>>,
    light: Option<Box<dyn ConversionBackend>>,
    config: EngineConfig,
    adaptive_use_light_model: bool,
}

/// Handle for the one worker thread.  `poll` and `submit` only take a mutex;
/// they never wait for model initialization or inference.
pub(crate) struct AsyncConversionWorker {
    shared: Arc<SharedWorkerState>,
    join: Option<JoinHandle<()>>,
}

impl AsyncConversionWorker {
    pub(crate) fn new() -> Self {
        Self::with_runtime(WorkerRuntime {
            factory: production_backend_factory(),
            main: None,
            light: None,
            config: EngineConfig::default(),
            adaptive_use_light_model: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_backend(backend: Box<dyn ConversionBackend>) -> Self {
        let name = backend.model_name().to_string();
        let worker = Self::with_runtime(WorkerRuntime {
            factory: production_backend_factory(),
            main: Some(backend),
            light: None,
            config: EngineConfig::default(),
            adaptive_use_light_model: false,
        });
        {
            let mut state = worker.shared.state.lock().expect("worker state poisoned");
            state.ready = true;
            state.model_name = name;
        }
        worker
    }

    #[cfg(test)]
    pub(crate) fn with_backend_factory(factory: BackendFactory) -> Self {
        Self::with_runtime(WorkerRuntime {
            factory,
            main: None,
            light: None,
            config: EngineConfig::default(),
            adaptive_use_light_model: false,
        })
    }

    fn with_runtime(runtime: WorkerRuntime) -> Self {
        let shared = Arc::new(SharedWorkerState {
            state: Mutex::new(WorkerState::default()),
            wake: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("karukan-conversion-worker".to_string())
            .spawn(move || worker_loop(thread_shared, runtime))
            .expect("failed to spawn conversion worker");
        Self {
            shared,
            join: Some(join),
        }
    }

    pub(crate) fn begin_init(&self, request: InitRequest) -> u64 {
        let mut state = self.shared.state.lock().expect("worker state poisoned");
        let previous_epoch = state.current_init_epoch;
        let previous_in_flight = state.initializing && previous_epoch != 0;
        state.current_init_epoch = state.current_init_epoch.wrapping_add(1);
        let epoch = state.current_init_epoch;
        if previous_in_flight {
            remember_init_result(
                &mut state,
                previous_epoch,
                Err("initialization request superseded".to_string()),
            );
        }
        if let Some((superseded_epoch, _)) = state.pending_init.replace((epoch, request)) {
            remember_init_result(
                &mut state,
                superseded_epoch,
                Err("initialization request superseded".to_string()),
            );
        }
        state.conversion_epoch = state.conversion_epoch.wrapping_add(1);
        state.completion = None;
        state.initializing = true;
        state.ready = false;
        state.init_error = None;
        state.model_name.clear();
        self.shared.wake.notify_one();
        epoch
    }

    pub(crate) fn wait_for_init(&self, epoch: u64) -> Result<(), String> {
        let mut state = self.shared.state.lock().expect("worker state poisoned");
        loop {
            if let Some(index) = state
                .init_results
                .iter()
                .position(|(result_epoch, _)| *result_epoch == epoch)
            {
                let (_, result) = state
                    .init_results
                    .remove(index)
                    .expect("init result index was present");
                return result;
            }
            if epoch != state.current_init_epoch {
                return Err("initialization outcome superseded or evicted".to_string());
            }
            state = self.shared.wake.wait(state).expect("worker state poisoned");
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .ready
    }

    pub(crate) fn is_initializing(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .initializing
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        let state = self.shared.state.lock().expect("worker state poisoned");
        state.pending_init.is_some()
            || state.initializing
            || state.converting
            || state.pending_conversion.is_some()
            || state.completion.is_some()
    }

    pub(crate) fn model_name(&self) -> String {
        let state = self.shared.state.lock().expect("worker state poisoned");
        if state.model_name.is_empty() {
            "unknown".to_string()
        } else {
            state.model_name.clone()
        }
    }

    /// Submit into a capacity-one latest-wins mailbox.
    pub(crate) fn submit(&self, request: ConversionRequest) {
        let mut state = self.shared.state.lock().expect("worker state poisoned");
        state.conversion_epoch = state.conversion_epoch.wrapping_add(1);
        state.pending_conversion = Some(request);
        state.completion = None;
        self.shared.wake.notify_one();
    }

    pub(crate) fn invalidate(&self) {
        let mut state = self.shared.state.lock().expect("worker state poisoned");
        state.conversion_epoch = state.conversion_epoch.wrapping_add(1);
        state.pending_conversion = None;
        state.completion = None;
        self.shared.wake.notify_all();
    }

    pub(crate) fn poll(&self) -> Option<ProposedCompletion> {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .completion
            .take()
    }

    #[cfg(test)]
    pub(crate) fn published_completion_generation(&self) -> Option<u64> {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .completion
            .as_ref()
            .map(|completion| completion.snapshot.generation)
    }

    #[cfg(test)]
    pub(crate) fn init_result_count(&self) -> usize {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .init_results
            .len()
    }

    #[cfg(test)]
    pub(crate) fn pending_conversion_count(&self) -> usize {
        usize::from(
            self.shared
                .state
                .lock()
                .expect("worker state poisoned")
                .pending_conversion
                .is_some(),
        )
    }

    #[cfg(test)]
    pub(crate) fn pending_conversion_generation(&self) -> Option<u64> {
        self.shared
            .state
            .lock()
            .expect("worker state poisoned")
            .pending_conversion
            .as_ref()
            .map(|request| request.snapshot.generation)
    }
}

impl Drop for AsyncConversionWorker {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("worker state poisoned");
        state.stopped = true;
        self.shared.wake.notify_one();
        drop(state);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(shared: Arc<SharedWorkerState>, mut runtime: WorkerRuntime) {
    loop {
        let (init, request) = {
            let mut state = shared.state.lock().expect("worker state poisoned");
            while !state.stopped
                && state.pending_init.is_none()
                && state.pending_conversion.is_none()
            {
                state = shared.wake.wait(state).expect("worker state poisoned");
            }
            if state.stopped {
                return;
            }
            let init = state.pending_init.take();
            let request = init.is_none().then(|| {
                state
                    .pending_conversion
                    .take()
                    .map(|request| (state.conversion_epoch, request))
            });
            let request = request.flatten();
            if request.is_some() {
                state.converting = true;
            }
            (init, request)
        };

        if let Some((init_epoch, init)) = init {
            let result = initialize_runtime(&mut runtime, &init);
            let mut state = shared.state.lock().expect("worker state poisoned");
            if state.current_init_epoch == init_epoch {
                remember_init_result(&mut state, init_epoch, result.clone());
                state.initializing = false;
                match result {
                    Ok(()) => {
                        state.ready = true;
                        state.init_error = None;
                        state.model_name = runtime
                            .main
                            .as_ref()
                            .map(|backend| backend.model_name().to_string())
                            .unwrap_or_default();
                    }
                    Err(error) => {
                        state.ready = false;
                        state.init_error = Some(error);
                        state.model_name.clear();
                    }
                }
            }
            shared.wake.notify_all();
            continue;
        }

        if let Some((conversion_epoch, request)) = request {
            let completion = if runtime.main.is_some() {
                convert_request(&mut runtime, request)
            } else {
                let chunk_cache = request
                    .chunks
                    .iter()
                    .map(|chunk| ComposingChunk {
                        position: chunk.position,
                        reading: chunk.reading.clone(),
                        converted: chunk.converted.clone(),
                        fresh: !chunk.should_convert,
                    })
                    .collect();
                ProposedCompletion {
                    snapshot: request.snapshot,
                    model_candidates: Vec::new(),
                    chunk_cache,
                    metrics: ProposedMetrics::default(),
                    ready: false,
                }
            };
            let mut state = shared.state.lock().expect("worker state poisoned");
            // A request may finish after invalidation or after a newer request
            // was submitted. Do not let that old completion occupy the
            // mailbox: the latest request epoch owns the completion slot.
            if state.conversion_epoch == conversion_epoch {
                runtime.adaptive_use_light_model = completion.metrics.adaptive_use_light_model;
                state.completion = Some(completion);
            }
            state.converting = false;
            shared.wake.notify_all();
        }
    }
}

fn remember_init_result(state: &mut WorkerState, epoch: u64, result: Result<(), String>) {
    if let Some((_, existing)) = state
        .init_results
        .iter_mut()
        .find(|(result_epoch, _)| *result_epoch == epoch)
    {
        *existing = result;
        return;
    }
    while state.init_results.len() >= MAX_INIT_RESULTS {
        state.init_results.pop_front();
    }
    state.init_results.push_back((epoch, result));
}

fn initialize_runtime(runtime: &mut WorkerRuntime, request: &InitRequest) -> Result<(), String> {
    runtime.main = None;
    runtime.light = None;
    runtime.adaptive_use_light_model = false;
    let main_variant = match request.strategy {
        StrategyMode::Light => request
            .light_variant
            .as_deref()
            .unwrap_or(&request.main_variant),
        StrategyMode::Main | StrategyMode::Adaptive => &request.main_variant,
    };
    let main = (runtime.factory)(main_variant, request.n_threads)?;
    let light = if request.strategy == StrategyMode::Adaptive {
        request
            .light_variant
            .as_deref()
            .and_then(|variant| (runtime.factory)(variant, request.n_threads).ok())
    } else {
        None
    };
    runtime.main = Some(main);
    runtime.light = light;
    runtime.config = request.config.clone();
    runtime.adaptive_use_light_model = false;
    Ok(())
}

fn convert_request(runtime: &mut WorkerRuntime, request: ConversionRequest) -> ProposedCompletion {
    let start = Instant::now();
    let normalized_reading = karukan_engine::hiragana_to_katakana(&request.snapshot.reading);
    let token_count = runtime
        .main
        .as_mut()
        .and_then(|backend| backend.count_input_tokens(&normalized_reading).ok());
    let strategy = determine_conversion_strategy(
        token_count.unwrap_or_default(),
        request.snapshot.candidate_count,
        runtime.light.is_some(),
        runtime.adaptive_use_light_model,
        &request.config,
    );

    let (model_candidates, chunk_cache) = match request.snapshot.request_kind {
        AsyncRequestKind::Explicit => (
            convert_with_strategy(
                runtime,
                &normalized_reading,
                &request.snapshot.context,
                &strategy,
            ),
            Vec::new(),
        ),
        AsyncRequestKind::AutoSuggest => {
            let (chunks, combined) = convert_chunks(runtime, &request);
            let candidates = (combined != request.snapshot.reading)
                .then_some(vec![combined])
                .unwrap_or_default();
            (candidates, chunks)
        }
    };

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let main_involved = matches!(
        strategy,
        ConversionStrategy::MainModelOnly
            | ConversionStrategy::MainModelBeam { .. }
            | ConversionStrategy::ParallelBeam { .. }
    );
    let proposed_adaptive_use_light_model = if main_involved
        && request.config.strategy == StrategyMode::Adaptive
        && request.config.max_latency_ms > 0
        && runtime.light.is_some()
    {
        elapsed_ms > request.config.max_latency_ms
    } else {
        runtime.adaptive_use_light_model
    };
    let main_name = runtime
        .main
        .as_ref()
        .map(|backend| backend.model_name().to_string())
        .unwrap_or_default();
    let light_name = runtime
        .light
        .as_ref()
        .map(|backend| backend.model_name().to_string())
        .unwrap_or_default();
    let model_name = match strategy {
        ConversionStrategy::ParallelBeam { .. } => {
            match (main_name.is_empty(), light_name.is_empty()) {
                (false, false) => format!("{main_name} + {light_name}"),
                (false, true) => main_name,
                (true, false) => light_name,
                (true, true) => String::new(),
            }
        }
        ConversionStrategy::LightModelOnly | ConversionStrategy::MainModelBeam { .. } => {
            if light_name.is_empty() {
                main_name
            } else {
                light_name
            }
        }
        ConversionStrategy::MainModelOnly => main_name,
    };

    ProposedCompletion {
        snapshot: request.snapshot,
        model_candidates,
        chunk_cache,
        metrics: ProposedMetrics {
            conversion_ms: elapsed_ms,
            model_name,
            adaptive_use_light_model: proposed_adaptive_use_light_model,
            token_count,
        },
        ready: true,
    }
}

fn convert_with_strategy(
    runtime: &mut WorkerRuntime,
    reading: &str,
    context: &str,
    strategy: &ConversionStrategy,
) -> Vec<String> {
    match strategy {
        ConversionStrategy::ParallelBeam { beam_width } => {
            let mut main_backend = runtime.main.take();
            let mut light_backend = runtime.light.take();
            let ((main_backend, main), (light_backend, light)) = std::thread::scope(|scope| {
                let main_thread = scope.spawn(move || {
                    let candidates = main_backend
                        .as_mut()
                        .map(|backend| backend.convert(reading, context, 1))
                        .unwrap_or_default();
                    (main_backend, candidates)
                });
                let light_thread = scope.spawn(move || {
                    let candidates = light_backend
                        .as_mut()
                        .map(|backend| backend.convert(reading, context, *beam_width))
                        .unwrap_or_default();
                    (light_backend, candidates)
                });
                (
                    main_thread.join().expect("main beam worker panicked"),
                    light_thread.join().expect("light beam worker panicked"),
                )
            });
            runtime.main = main_backend;
            runtime.light = light_backend;
            super::InputMethodEngine::merge_candidates_dedup(main, light, *beam_width)
        }
        ConversionStrategy::LightModelOnly => runtime
            .light
            .as_mut()
            .or(runtime.main.as_mut())
            .map(|backend| backend.convert(reading, context, 1))
            .unwrap_or_default(),
        ConversionStrategy::MainModelOnly => runtime
            .main
            .as_mut()
            .map(|backend| backend.convert(reading, context, 1))
            .unwrap_or_default(),
        ConversionStrategy::MainModelBeam { beam_width } => runtime
            .main
            .as_mut()
            .map(|backend| backend.convert(reading, context, *beam_width))
            .unwrap_or_default(),
    }
}

fn convert_chunks(
    runtime: &mut WorkerRuntime,
    request: &ConversionRequest,
) -> (Vec<ComposingChunk>, String) {
    let mut chunks = Vec::new();
    let mut combined = String::new();
    for chunk in &request.chunks {
        let converted = if chunk.should_convert {
            let normalized_reading = karukan_engine::hiragana_to_katakana(&chunk.reading);
            let token_count = runtime
                .main
                .as_mut()
                .and_then(|backend| backend.count_input_tokens(&normalized_reading).ok())
                .unwrap_or_default();
            let strategy = determine_conversion_strategy(
                token_count,
                1,
                runtime.light.is_some(),
                runtime.adaptive_use_light_model,
                &request.config,
            );
            let candidates =
                convert_with_strategy(runtime, &normalized_reading, &chunk.context, &strategy);
            candidates
                .into_iter()
                .next()
                .unwrap_or_else(|| chunk.converted.clone())
        } else {
            chunk.converted.clone()
        };
        combined.push_str(&converted);
        chunks.push(ComposingChunk {
            position: chunk.position,
            reading: chunk.reading.clone(),
            converted,
            fresh: true,
        });
    }
    (chunks, combined)
}

impl super::InputMethodEngine {
    /// Queue a complete handler snapshot.  This method performs no model work
    /// and replaces any not-yet-started request in the capacity-one mailbox.
    pub(super) fn submit_async_conversion(
        &mut self,
        request_kind: AsyncRequestKind,
        skip_learning: bool,
    ) {
        let reading = self.input_buf.text.clone();
        if reading.is_empty()
            || self.input_mode == InputMode::Katakana
            || !karukan_engine::contains_kana(&reading)
        {
            return;
        }
        let input_state = match &self.state {
            super::InputState::Empty => AsyncInputState::Empty,
            super::InputState::Composing { .. } => AsyncInputState::Composing,
            super::InputState::Conversion { .. } => AsyncInputState::Conversion,
        };
        let candidate_count = match request_kind {
            AsyncRequestKind::AutoSuggest => 1,
            AsyncRequestKind::Explicit => self.config.num_candidates,
        };
        let snapshot = ConversionSnapshot {
            generation: self.generation,
            reading,
            context: self.truncate_context_for_api(),
            mode: self.input_mode,
            input_state,
            request_kind,
            candidate_count,
            skip_learning,
            chunk_cache_epoch: self.chunk_cache_epoch,
        };
        self.worker.submit(ConversionRequest {
            snapshot,
            chunks: if request_kind == AsyncRequestKind::AutoSuggest {
                self.chunk_requests()
            } else {
                Vec::new()
            },
            config: self.config.clone(),
        });
        self.pending_async_request = Some(PendingRequestMetadata {
            request_kind,
            candidate_count,
            skip_learning,
        });
    }

    /// Poll a completion without waiting.  A completion is committed only if
    /// every snapshot field still describes the current engine state.
    pub fn poll_async_conversion(&mut self) -> Option<super::EngineResult> {
        self.poll_init_assets();
        let completion = self.worker.poll()?;
        if !self.completion_matches(&completion.snapshot) || !completion.ready {
            return None;
        }

        self.pending_async_request = None;
        self.metrics.conversion_ms = completion.metrics.conversion_ms;
        self.metrics.model_name = completion.metrics.model_name.clone();
        self.metrics.adaptive_use_light_model = completion.metrics.adaptive_use_light_model;
        self.metrics.token_count = completion.metrics.token_count;
        self.converters.kanji = completion
            .metrics
            .token_count
            .map(|token_count| super::CompatibilityTokenCounter { token_count });

        match completion.snapshot.request_kind {
            AsyncRequestKind::AutoSuggest => Some(self.apply_auto_completion(completion)),
            AsyncRequestKind::Explicit => Some(self.apply_explicit_completion(completion)),
        }
    }

    fn completion_matches(&self, snapshot: &ConversionSnapshot) -> bool {
        let Some(metadata) = self.pending_async_request else {
            return false;
        };
        let input_state = match &self.state {
            super::InputState::Empty => AsyncInputState::Empty,
            super::InputState::Composing { .. } => AsyncInputState::Composing,
            super::InputState::Conversion { .. } => AsyncInputState::Conversion,
        };
        snapshot
            == &ConversionSnapshot {
                generation: self.generation,
                reading: self.input_buf.text.clone(),
                context: self.truncate_context_for_api(),
                mode: self.input_mode,
                input_state,
                request_kind: metadata.request_kind,
                candidate_count: metadata.candidate_count,
                skip_learning: metadata.skip_learning,
                chunk_cache_epoch: self.chunk_cache_epoch,
            }
    }

    fn apply_auto_completion(&mut self, completion: ProposedCompletion) -> super::EngineResult {
        self.chunks = completion.chunk_cache;
        let reading = completion.snapshot.reading;
        let model_candidates: Vec<super::Candidate> = completion
            .model_candidates
            .into_iter()
            .filter(|text| text != &reading)
            .map(|text| super::Candidate {
                text,
                reading: Some(reading.clone()),
                source_label: Some(super::CandidateSource::Model.label().to_string()),
                description: None,
            })
            .collect();

        let mut all_candidates = self.lookup_learning_candidates(&reading);
        append_candidates_dedup(&mut all_candidates, model_candidates.clone());
        append_candidates_dedup(&mut all_candidates, self.lookup_dict_candidates(&reading));
        append_candidates_dedup(&mut all_candidates, self.lookup_rewriter_variants(&reading));

        if self.live.enabled && self.input_mode != InputMode::Katakana {
            self.live.text = karukan_engine::rewriter::NumberRewriter::new()
                .preferred_decimal_counter_surface(&reading)
                .or_else(|| {
                    model_candidates
                        .first()
                        .map(|candidate| candidate.text.clone())
                })
                .unwrap_or_default();
        }
        let preedit = self.set_composing_state();
        let mut result = super::EngineResult::not_consumed()
            .with_action(super::EngineAction::UpdatePreedit(preedit))
            .with_action(super::EngineAction::UpdateAuxText(
                self.format_aux_suggest(&reading),
            ));
        if self.live.enabled && matches!(self.input_mode, InputMode::Hiragana | InputMode::Alphabet)
        {
            // Live conversion renders the selected projection in the preedit.
            // Keep ordinary candidates hidden until the user explicitly starts
            // conversion with Space/Down/Tab; Emoji remains outside this branch.
            result.actions.push(super::EngineAction::HideCandidates);
        } else if all_candidates.is_empty() {
            result.actions.push(super::EngineAction::HideCandidates);
        } else {
            result.actions.push(super::EngineAction::ShowCandidates(
                super::CandidateList::new(all_candidates),
            ));
        }
        result
    }

    fn apply_explicit_completion(&mut self, completion: ProposedCompletion) -> super::EngineResult {
        let Some(current) = self.state.candidates().cloned() else {
            return super::EngineResult::not_consumed();
        };
        let selected = current.selected().cloned();
        let mut candidates = current.candidates().to_vec();
        let filter_script_variants =
            self.should_filter_sentence_script_variants(&completion.snapshot.reading);
        for text in completion.model_candidates {
            if filter_script_variants
                && super::conversion::is_pure_script_variant(&text, &completion.snapshot.reading)
            {
                continue;
            }
            if !candidates.iter().any(|candidate| candidate.text == text) {
                candidates.push(super::Candidate {
                    text,
                    reading: Some(completion.snapshot.reading.clone()),
                    source_label: Some(super::CandidateSource::Model.label().to_string()),
                    description: None,
                });
            }
        }
        let mut updated = super::CandidateList::new(candidates);
        if updated.is_empty() {
            let reading = completion.snapshot.reading;
            let preedit = super::Preedit::with_text_underlined(&reading);
            self.state = super::InputState::Conversion {
                preedit: preedit.clone(),
                candidates: updated,
            };
            return super::EngineResult::not_consumed()
                .with_action(super::EngineAction::UpdatePreedit(preedit))
                .with_action(super::EngineAction::HideCandidates)
                .with_action(super::EngineAction::UpdateAuxText(
                    self.format_aux_conversion_with_page(&reading, None),
                ));
        }
        if let Some(selected) = selected
            && let Some(index) = updated.candidates().iter().position(|candidate| {
                candidate.text == selected.text && candidate.reading == selected.reading
            })
        {
            let _ = updated.select(index);
        }
        let selected_text = updated.selected_text().unwrap_or("").to_string();
        let reading = updated
            .selected()
            .and_then(|candidate| candidate.reading.as_deref())
            .unwrap_or(&completion.snapshot.reading);
        let preedit = super::Preedit::from_segments(
            vec![super::PreeditSegment::highlighted(&selected_text)],
            selected_text.chars().count(),
        );
        self.state = super::InputState::Conversion {
            preedit: preedit.clone(),
            candidates: updated.clone(),
        };
        super::EngineResult::not_consumed()
            .with_action(super::EngineAction::UpdatePreedit(preedit))
            .with_action(super::EngineAction::ShowCandidates(updated.clone()))
            .with_action(super::EngineAction::UpdateAuxText(
                self.format_aux_conversion_with_page(reading, Some(&updated)),
            ))
    }
}

fn append_candidates_dedup(target: &mut Vec<super::Candidate>, source: Vec<super::Candidate>) {
    for candidate in source {
        if !target
            .iter()
            .any(|existing| existing.text == candidate.text)
        {
            target.push(candidate);
        }
    }
}
