//! Engine initialization (model loading, dictionary setup)

use anyhow::{Context, Result};
use std::thread::{self, JoinHandle};
use tracing::debug;

use crate::config::settings::StrategyMode;

use super::*;

pub(super) struct InitAssets {
    system: Option<Dictionary>,
    user: Option<Dictionary>,
    learning: Option<LearningCache>,
}

fn load_system_dictionary(dict_path: Option<&str>) -> Option<Dictionary> {
    let path = if let Some(p) = dict_path {
        std::path::PathBuf::from(p)
    } else if let Some(data_dir) = Settings::data_dir() {
        data_dir.join("dict.bin")
    } else {
        debug!("Could not determine data directory for system dictionary");
        return None;
    };

    if !path.exists() {
        debug!("System dictionary not found at {:?}, skipping", path);
        return None;
    }

    match Dictionary::load(&path) {
        Ok(dict) => {
            debug!("System dictionary loaded from {:?}", path);
            Some(dict)
        }
        Err(e) => {
            debug!("Failed to load system dictionary from {:?}: {}", path, e);
            None
        }
    }
}

fn load_learning_cache(enabled: bool, max_entries: usize) -> Option<LearningCache> {
    if !enabled {
        return None;
    }

    let Some(path) = Settings::learning_file() else {
        debug!("Could not determine learning cache path");
        return Some(LearningCache::new(max_entries));
    };

    if path.exists() {
        match LearningCache::load(&path, max_entries) {
            Ok(cache) => {
                debug!(
                    "Learning cache loaded from {:?} ({} entries)",
                    path,
                    cache.entry_count()
                );
                Some(cache)
            }
            Err(e) => {
                debug!("Failed to load learning cache from {:?}: {}", path, e);
                Some(LearningCache::new(max_entries))
            }
        }
    } else {
        debug!("Learning cache not found at {:?}, starting empty", path);
        Some(LearningCache::new(max_entries))
    }
}

fn load_user_dictionaries() -> Option<Dictionary> {
    let Some(dir) = Settings::user_dict_dir() else {
        debug!("Could not determine user dictionary directory");
        return None;
    };

    if !dir.exists() {
        debug!(
            "User dictionary directory {:?} does not exist, skipping",
            dir
        );
        return None;
    }

    let Ok(entries) = std::fs::read_dir(&dir) else {
        debug!("Failed to read user dictionary directory {:?}", dir);
        return None;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();

    if paths.is_empty() {
        debug!("No files in user dictionary directory {:?}", dir);
        return None;
    }

    paths.sort();
    let mut dicts = Vec::new();
    for path in &paths {
        match Dictionary::load_auto(path) {
            Ok(dict) => {
                debug!("User dictionary loaded from {:?}", path);
                dicts.push(dict);
            }
            Err(e) => {
                debug!("Failed to load user dictionary from {:?}: {}", path, e);
            }
        }
    }

    match Dictionary::merge(dicts) {
        Ok(Some(merged)) => {
            debug!(
                "User dictionaries merged successfully ({} files from {:?})",
                paths.len(),
                dir
            );
            Some(merged)
        }
        Ok(None) => None,
        Err(e) => {
            debug!("Failed to merge user dictionaries: {}", e);
            None
        }
    }
}

impl InputMethodEngine {
    /// Full engine initialization from user settings: system dictionary,
    /// user dictionaries, learning cache, and conversion models according
    /// to the configured strategy.
    ///
    /// Shared by the fcitx5 FFI (`karukan_engine_init`) and the stdio
    /// JSON-RPC server (`init` method). In `Adaptive` mode a light-model
    /// failure is non-fatal (beam search is simply unavailable).
    pub fn init_from_settings(&mut self, settings: &Settings) -> Result<()> {
        let init_epoch = self.queue_init_from_settings(settings)?;
        let result = self
            .worker
            .wait_for_init(init_epoch)
            .map_err(|error| anyhow::anyhow!(error));
        self.wait_for_init_assets();
        result?;
        tracing::info!("Karukan init complete: {}", self.model_name());
        Ok(())
    }

    /// Start model initialization without waiting on the handler thread.
    pub fn begin_init_from_settings(&mut self, settings: &Settings) -> Result<()> {
        self.queue_init_from_settings(settings).map(|_| ())
    }

    fn queue_init_from_settings(&mut self, settings: &Settings) -> Result<u64> {
        let strategy = settings.conversion.strategy;
        tracing::info!(
            "Karukan init: model={:?}, light_model={:?}, strategy={:?}",
            settings.conversion.model,
            settings.conversion.light_model,
            strategy,
        );

        let n_threads = settings.conversion.n_threads;

        let main_variant = resolve_variant_id(settings.conversion.model.as_deref())
            .context("invalid model settings")?;
        let light_variant = match strategy {
            StrategyMode::Adaptive | StrategyMode::Light => {
                match resolve_variant_id(settings.conversion.light_model.as_deref()) {
                    Ok(id) => Some(id),
                    Err(error) if strategy == StrategyMode::Adaptive => {
                        tracing::warn!("Invalid light_model settings, using default: {}", error);
                        Some(karukan_engine::kanji::registry().default_model.clone())
                    }
                    Err(error) => return Err(error).context("invalid light_model settings"),
                }
            }
            StrategyMode::Main => None,
        };
        self.queue_init_assets(settings);
        let init_epoch = self.worker.begin_init(async_conversion::InitRequest {
            main_variant,
            light_variant,
            strategy,
            n_threads,
            config: EngineConfig::from_settings(settings),
        });
        Ok(init_epoch)
    }

    fn queue_init_assets(&mut self, settings: &Settings) {
        self.poll_init_assets();
        if self.pending_init_assets.is_some() {
            return;
        }

        let load_system = self.dicts.system.is_none();
        let load_user = self.dicts.user.is_none();
        let load_learning = settings.learning.enabled && self.learning.is_none();
        let settings = settings.clone();
        self.pending_init_assets = Some(thread::spawn(move || InitAssets {
            system: load_system
                .then(|| load_system_dictionary(settings.conversion.dict_path.as_deref()))
                .flatten(),
            user: load_user.then(load_user_dictionaries).flatten(),
            learning: load_learning
                .then(|| {
                    load_learning_cache(settings.learning.enabled, settings.learning.max_entries)
                })
                .flatten(),
        }));
    }

    pub(super) fn poll_init_assets(&mut self) {
        let finished = self
            .pending_init_assets
            .as_ref()
            .map(JoinHandle::is_finished)
            .unwrap_or(false);
        if !finished {
            return;
        }

        let Some(handle) = self.pending_init_assets.take() else {
            return;
        };
        match handle.join() {
            Ok(assets) => {
                if self.dicts.system.is_none() {
                    self.dicts.system = assets.system;
                }
                if self.dicts.user.is_none() {
                    self.dicts.user = assets.user;
                }
                if self.learning.is_none() {
                    self.learning = assets.learning;
                }
            }
            Err(_) => tracing::warn!("Karukan initialization asset loader panicked"),
        }
    }

    fn wait_for_init_assets(&mut self) {
        let Some(handle) = self.pending_init_assets.take() else {
            return;
        };
        match handle.join() {
            Ok(assets) => {
                if self.dicts.system.is_none() {
                    self.dicts.system = assets.system;
                }
                if self.dicts.user.is_none() {
                    self.dicts.user = assets.user;
                }
                if self.learning.is_none() {
                    self.learning = assets.learning;
                }
            }
            Err(_) => tracing::warn!("Karukan initialization asset loader panicked"),
        }
    }

    /// Initialize the kanji converter (call this early to avoid latency)
    /// Uses the default model from the registry.
    pub fn init_kanji_converter(&mut self) -> Result<()> {
        let default_id = karukan_engine::kanji::registry().default_model.clone();
        self.init_kanji_converter_with_model(&default_id, 0)
    }

    /// Initialize the kanji converter with a specific variant id
    pub fn init_kanji_converter_with_model(
        &mut self,
        variant_id: &str,
        n_threads: u32,
    ) -> Result<()> {
        let main_variant = resolve_variant_id(Some(variant_id))?;
        let init_epoch = self.worker.begin_init(async_conversion::InitRequest {
            main_variant,
            light_variant: None,
            strategy: StrategyMode::Main,
            n_threads,
            config: self.config.clone(),
        });
        self.worker
            .wait_for_init(init_epoch)
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(())
    }

    /// Initialize the light model for beam search (generates multiple candidates on Space conversion)
    pub fn init_light_kanji_converter(&mut self, variant_id: &str, n_threads: u32) -> Result<()> {
        let main_variant = karukan_engine::kanji::registry().default_model.clone();
        let light_variant = resolve_variant_id(Some(variant_id))?;
        let init_epoch = self.worker.begin_init(async_conversion::InitRequest {
            main_variant,
            light_variant: Some(light_variant),
            strategy: StrategyMode::Adaptive,
            n_threads,
            config: self.config.clone(),
        });
        self.worker
            .wait_for_init(init_epoch)
            .map_err(|error| anyhow::anyhow!(error))?;
        Ok(())
    }

    /// Initialize the system dictionary for candidate lookup
    ///
    /// Uses `dict_path` from settings if specified, otherwise defaults to `data_dir/dict.bin`.
    /// If the file doesn't exist, the engine continues without a dictionary.
    pub fn init_system_dictionary(&mut self, dict_path: Option<&str>) {
        if self.dicts.system.is_some() {
            return;
        }

        let path = if let Some(p) = dict_path {
            std::path::PathBuf::from(p)
        } else if let Some(data_dir) = Settings::data_dir() {
            data_dir.join("dict.bin")
        } else {
            debug!("Could not determine data directory for system dictionary");
            return;
        };

        if !path.exists() {
            debug!("System dictionary not found at {:?}, skipping", path);
            return;
        }

        match Dictionary::load(&path) {
            Ok(dict) => {
                debug!("System dictionary loaded from {:?}", path);
                self.dicts.system = Some(dict);
            }
            Err(e) => {
                debug!("Failed to load system dictionary from {:?}: {}", path, e);
            }
        }
    }

    /// Initialize the learning cache from disk.
    ///
    /// Loads `~/.local/share/karukan-im/learning.tsv` if it exists.
    /// If the file doesn't exist, creates an empty in-memory cache.
    pub fn init_learning_cache(&mut self, enabled: bool, max_entries: usize) {
        if !enabled || self.learning.is_some() {
            return;
        }

        let Some(path) = Settings::learning_file() else {
            debug!("Could not determine learning cache path");
            self.learning = Some(LearningCache::new(max_entries));
            return;
        };

        if path.exists() {
            match LearningCache::load(&path, max_entries) {
                Ok(cache) => {
                    debug!(
                        "Learning cache loaded from {:?} ({} entries)",
                        path,
                        cache.entry_count()
                    );
                    self.learning = Some(cache);
                }
                Err(e) => {
                    debug!("Failed to load learning cache from {:?}: {}", path, e);
                    self.learning = Some(LearningCache::new(max_entries));
                }
            }
        } else {
            debug!("Learning cache not found at {:?}, starting empty", path);
            self.learning = Some(LearningCache::new(max_entries));
        }
    }

    /// Initialize user dictionaries by scanning the user dictionary directory.
    ///
    /// All files in the directory are loaded with `Dictionary::load_auto()`
    /// (auto-detects KRKN binary or Mozc TSV). Files are loaded in sorted
    /// order; earlier files have higher priority after merging.
    ///
    /// Default directory: `~/.local/share/karukan-im/user_dicts/`
    pub fn init_user_dictionaries(&mut self) {
        if self.dicts.user.is_some() {
            return;
        }

        let Some(dir) = Settings::user_dict_dir() else {
            debug!("Could not determine user dictionary directory");
            return;
        };

        if !dir.exists() {
            debug!(
                "User dictionary directory {:?} does not exist, skipping",
                dir
            );
            return;
        }

        let Ok(entries) = std::fs::read_dir(&dir) else {
            debug!("Failed to read user dictionary directory {:?}", dir);
            return;
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();

        if paths.is_empty() {
            debug!("No files in user dictionary directory {:?}", dir);
            return;
        }

        // Sort for deterministic load order (alphabetical)
        paths.sort();

        let mut dicts = Vec::new();
        for path in &paths {
            match Dictionary::load_auto(path) {
                Ok(dict) => {
                    debug!("User dictionary loaded from {:?}", path);
                    dicts.push(dict);
                }
                Err(e) => {
                    debug!("Failed to load user dictionary from {:?}: {}", path, e);
                }
            }
        }

        if dicts.is_empty() {
            return;
        }

        match Dictionary::merge(dicts) {
            Ok(Some(merged)) => {
                debug!(
                    "User dictionaries merged successfully ({} files from {:?})",
                    paths.len(),
                    dir
                );
                self.dicts.user = Some(merged);
            }
            Ok(None) => {}
            Err(e) => {
                debug!("Failed to merge user dictionaries: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod init_asset_tests {
    use super::*;
    use std::sync::mpsc;

    fn test_dictionary() -> Dictionary {
        let file = tempfile::NamedTempFile::new().expect("create dictionary fixture");
        std::fs::write(
            file.path(),
            r#"[{"reading":"てすと","candidates":[{"surface":"試験","score":1.0}]}]"#,
        )
        .expect("write dictionary fixture");
        Dictionary::build_from_json(file.path()).expect("build dictionary fixture")
    }

    #[test]
    fn rapid_retries_retain_the_in_flight_asset_loader() {
        let mut engine = InputMethodEngine::new();
        engine.dicts.system = Some(test_dictionary());
        engine.dicts.user = Some(test_dictionary());
        let mut settings = Settings::default();
        settings.learning.enabled = false;

        let (release_tx, release_rx) = mpsc::channel();
        let loader = thread::spawn(move || {
            let _ = release_rx.recv();
            InitAssets {
                system: None,
                user: None,
                learning: None,
            }
        });
        let loader_id = loader.thread().id();
        engine.pending_init_assets = Some(loader);

        for _ in 0..8 {
            engine.queue_init_assets(&settings);
        }

        let retained_id = engine
            .pending_init_assets
            .as_ref()
            .expect("asset loader remains owned")
            .thread()
            .id();
        assert_eq!(retained_id, loader_id);

        release_tx.send(()).expect("release asset loader");
        engine.wait_for_init_assets();
    }
}
