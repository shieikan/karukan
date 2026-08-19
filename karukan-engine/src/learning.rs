//! Learning cache for remembering user-selected conversion results.
//!
//! Records which surface forms the user chose for each reading, and
//! boosts those candidates on subsequent conversions. Persisted as a
//! simple TSV file (`reading\tsurface\tfrequency\tlast_access`).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// A single learned conversion entry.
#[derive(Debug, Clone)]
pub struct LearningEntry {
    /// Surface form (e.g. "今日")
    pub surface: String,
    /// Number of times this surface was selected
    pub frequency: u32,
    /// Last selection time as Unix timestamp (seconds)
    pub last_access: u64,
}

/// Size limits for a [`LearningCache`].
///
/// Passed whole at construction ([`LearningCache::new`] /
/// [`LearningCache::load`]) so a caller can't apply one limit and forget the
/// other — there is no post-construction setter to miss.
#[derive(Debug, Clone, Copy)]
pub struct LearningConfig {
    /// Maximum number of total entries across all readings; lowest-score
    /// entries are evicted on save when over this limit.
    pub max_entries: usize,
    /// Maximum surface length (Unicode chars) that [`LearningCache::record`]
    /// accepts. Keeps whole-sentence live-conversion commits — one-off text
    /// that never matches again — out of the cache.
    pub max_surface_chars: usize,
    /// Whole-day TTL for frequency-one entries; zero disables TTL pruning.
    pub singleton_ttl_days: u64,
}

impl LearningConfig {
    /// Default for [`max_entries`](Self::max_entries).
    pub const DEFAULT_MAX_ENTRIES: usize = 10_000;
    /// Default for [`max_surface_chars`](Self::max_surface_chars).
    pub const DEFAULT_MAX_SURFACE_CHARS: usize = 50;
    /// Default number of whole days before an unused singleton expires.
    pub const DEFAULT_SINGLETON_TTL_DAYS: u64 = 7;
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            max_entries: Self::DEFAULT_MAX_ENTRIES,
            max_surface_chars: Self::DEFAULT_MAX_SURFACE_CHARS,
            singleton_ttl_days: Self::DEFAULT_SINGLETON_TTL_DAYS,
        }
    }
}

/// In-memory cache of user learning data.
///
/// Keyed by reading (hiragana). Each reading maps to a list of surface
/// entries with frequency and recency metadata.
#[derive(Debug)]
pub struct LearningCache {
    entries: HashMap<String, Vec<LearningEntry>>,
    max_entries: usize,
    max_surface_chars: usize,
    singleton_ttl_days: u64,
    dirty: bool,
}

impl LearningCache {
    /// Create an empty cache with the given limits.
    pub fn new(config: LearningConfig) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: config.max_entries,
            max_surface_chars: config.max_surface_chars,
            singleton_ttl_days: config.singleton_ttl_days,
            dirty: false,
        }
    }

    /// Record a user selection. Increments frequency and updates last_access.
    ///
    /// Surfaces longer than `max_surface_chars` are skipped; see
    /// [`LearningConfig::max_surface_chars`] for why.
    pub fn record(&mut self, reading: &str, surface: &str) {
        if surface.chars().count() > self.max_surface_chars {
            return;
        }
        let now = now_unix();
        let entries = self.entries.entry(reading.to_string()).or_default();

        if let Some(entry) = entries.iter_mut().find(|e| e.surface == surface) {
            entry.frequency += 1;
            entry.last_access = now;
        } else {
            entries.push(LearningEntry {
                surface: surface.to_string(),
                frequency: 1,
                last_access: now,
            });
        }
        self.dirty = true;
    }

    /// Remove every learned entry that would resurface `surface` for input
    /// `reading`: the exact-reading entry plus every longer reading with
    /// `reading` as a prefix (the [`prefix_lookup`](Self::prefix_lookup)
    /// fan-out) — an exact-only delete would leave a twin that pops back on
    /// the next conversion. Returns whether anything was removed; persisted
    /// at the next `save`.
    pub fn remove_suggestion(&mut self, reading: &str, surface: &str) -> bool {
        let mut removed = false;
        self.entries.retain(|r, entries| {
            if !r.starts_with(reading) {
                return true;
            }
            let before = entries.len();
            entries.retain(|e| e.surface != surface);
            removed |= entries.len() != before;
            !entries.is_empty()
        });
        if removed {
            self.dirty = true;
        }
        removed
    }

    /// Exact-match lookup: returns `(surface, score)` pairs sorted by score descending.
    pub fn lookup(&self, reading: &str) -> Vec<(String, f64)> {
        let now = now_unix();
        let Some(entries) = self.entries.get(reading) else {
            return Vec::new();
        };
        let mut scored: Vec<(String, f64)> = entries
            .iter()
            .map(|e| (e.surface.clone(), score(e, now)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }

    /// Exact-match lookup restricted to entries at or above `min_frequency`.
    /// Results retain the same score ordering as [`Self::lookup`].
    pub fn lookup_min_frequency(&self, reading: &str, min_frequency: u32) -> Vec<(String, f64)> {
        let now = now_unix();
        let Some(entries) = self.entries.get(reading) else {
            return Vec::new();
        };
        let mut scored: Vec<(String, f64)> = entries
            .iter()
            .filter(|entry| entry.frequency >= min_frequency)
            .map(|entry| (entry.surface.clone(), score(entry, now)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }

    /// Prefix-match lookup: returns `(reading, surface, score)` triples
    /// for all readings that start with `prefix`, sorted by score descending.
    pub fn prefix_lookup(&self, prefix: &str) -> Vec<(String, String, f64)> {
        let now = now_unix();
        let mut results: Vec<(String, String, f64)> = Vec::new();
        for (reading, entries) in &self.entries {
            if reading.starts_with(prefix) {
                for entry in entries {
                    results.push((reading.clone(), entry.surface.clone(), score(entry, now)));
                }
            }
        }
        results.sort_by(|a, b| b.2.total_cmp(&a.2));
        results
    }

    /// Load a learning cache from a TSV file.
    ///
    /// Format: `reading\tsurface\tfrequency\tlast_access`
    /// Lines starting with `#` are comments.
    pub fn load(path: &Path, config: LearningConfig) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let mut cache = Self::new(config);

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 4 {
                continue;
            }
            let reading = parts[0];
            let surface = parts[1];
            let frequency: u32 = match parts[2].parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let last_access: u64 = match parts[3].parse() {
                Ok(v) => v,
                Err(_) => continue,
            };

            cache
                .entries
                .entry(reading.to_string())
                .or_default()
                .push(LearningEntry {
                    surface: surface.to_string(),
                    frequency,
                    last_access,
                });
        }

        // Not dirty — just loaded from disk
        cache.dirty = false;
        Ok(cache)
    }

    /// Save the cache to a TSV file, evicting low-score entries if over capacity.
    pub fn save(&mut self, path: &Path) -> anyhow::Result<()> {
        self.save_with_rename(path, |temporary, destination| {
            std::fs::rename(temporary, destination)
        })
    }

    /// Save the cache with an injected timestamp for deterministic tests.
    #[cfg(test)]
    fn save_at(&mut self, path: &Path, now: u64) -> anyhow::Result<()> {
        self.save_with_rename_at(path, now, |temporary, destination| {
            std::fs::rename(temporary, destination)
        })
    }

    /// Save through a same-directory temporary file using the current time.
    fn save_with_rename<R>(&mut self, path: &Path, rename: R) -> anyhow::Result<()>
    where
        R: Fn(&Path, &Path) -> std::io::Result<()>,
    {
        self.save_with_rename_at(path, now_unix(), rename)
    }

    /// Save through a same-directory temporary file with an injected time.
    fn save_with_rename_at<R>(&mut self, path: &Path, now: u64, rename: R) -> anyhow::Result<()>
    where
        R: Fn(&Path, &Path) -> std::io::Result<()>,
    {
        let parent = normalized_parent_dir(path);
        std::fs::create_dir_all(parent)?;

        let mut persisted = Self {
            entries: self.entries.clone(),
            max_entries: self.max_entries,
            max_surface_chars: self.max_surface_chars,
            singleton_ttl_days: self.singleton_ttl_days,
            dirty: self.dirty,
        };
        persisted.prune_expired_singletons(now);
        persisted.evict_at(now);

        let (file, temporary_path) = create_temporary_file(path)?;
        let write_result = (|| -> anyhow::Result<()> {
            let mut writer = std::io::BufWriter::new(file);
            writeln!(writer, "# karukan learning cache v1")?;

            // Sort readings for deterministic output
            let mut readings: Vec<&String> = persisted.entries.keys().collect();
            readings.sort();

            for reading in readings {
                if let Some(entries) = persisted.entries.get(reading) {
                    for entry in entries {
                        writeln!(
                            writer,
                            "{}\t{}\t{}\t{}",
                            reading, entry.surface, entry.frequency, entry.last_access
                        )?;
                    }
                }
            }

            writer.flush()?;
            let file = writer.into_inner().map_err(|error| error.into_error())?;
            file.sync_all()?;
            drop(file);
            rename(&temporary_path, path)?;
            // Best-effort directory barrier after the atomic replacement. Some
            // platforms/filesystems do not support syncing directory handles.
            let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
            Ok(())
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
            return write_result;
        }

        self.entries = persisted.entries;
        self.dirty = false;
        Ok(())
    }

    /// Whether there are unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether saving is needed because the cache changed or a singleton has
    /// expired under the current wall clock.
    pub fn needs_save(&self) -> bool {
        self.needs_save_at(now_unix())
    }

    /// Deterministic counterpart to [`needs_save`](Self::needs_save).
    pub fn needs_save_at(&self, now: u64) -> bool {
        if self.dirty {
            return true;
        }

        let Some(ttl_seconds) = self.singleton_ttl_seconds() else {
            return false;
        };
        self.entries.values().any(|entries| {
            entries
                .iter()
                .any(|entry| singleton_expired(entry, now, ttl_seconds))
        })
    }

    /// Total number of (reading, surface) pairs across all readings.
    pub fn entry_count(&self) -> usize {
        self.entries.values().map(|v| v.len()).sum()
    }

    /// Evict lowest-score entries using an injected timestamp.
    fn evict_at(&mut self, now: u64) {
        let total = self.entry_count();
        if total <= self.max_entries {
            return;
        }

        // Collect all entries with their (reading, index, score)
        let mut all: Vec<(String, usize, f64)> = Vec::with_capacity(total);
        for (reading, entries) in &self.entries {
            for (i, entry) in entries.iter().enumerate() {
                all.push((reading.clone(), i, score(entry, now)));
            }
        }
        // Sort by score ascending (lowest first = eviction candidates)
        all.sort_by(|a, b| a.2.total_cmp(&b.2));

        let to_remove = total - self.max_entries;
        // Collect indices to remove, grouped by reading
        let mut remove_set: HashMap<String, Vec<usize>> = HashMap::new();
        for &(ref reading, idx, _) in all.iter().take(to_remove) {
            remove_set.entry(reading.clone()).or_default().push(idx);
        }

        // Remove entries in reverse index order to preserve indices
        for (reading, indices) in &mut remove_set {
            indices.sort_unstable();
            indices.reverse();
            if let Some(entries) = self.entries.get_mut(reading) {
                for &idx in indices.iter() {
                    if idx < entries.len() {
                        entries.remove(idx);
                    }
                }
                if entries.is_empty() {
                    self.entries.remove(reading);
                }
            }
        }
    }

    /// Remove expired singleton entries before applying the capacity limit.
    fn prune_expired_singletons(&mut self, now: u64) {
        let Some(ttl_seconds) = self.singleton_ttl_seconds() else {
            return;
        };

        self.entries.retain(|_, entries| {
            entries.retain(|entry| !singleton_expired(entry, now, ttl_seconds));
            !entries.is_empty()
        });
    }

    fn singleton_ttl_seconds(&self) -> Option<u64> {
        (self.singleton_ttl_days != 0)
            .then(|| self.singleton_ttl_days.saturating_mul(SECONDS_PER_DAY))
    }
}

const MAX_TEMPORARY_FILE_ATTEMPTS: usize = 16;
const SECONDS_PER_DAY: u64 = 86_400;

fn singleton_expired(entry: &LearningEntry, now: u64, ttl_seconds: u64) -> bool {
    entry.frequency == 1 && now.saturating_sub(entry.last_access) > ttl_seconds
}

fn normalized_parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn temporary_path(path: &Path, attempt: usize) -> PathBuf {
    let parent = normalized_parent_dir(path);
    let mut file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "learning-cache".into());
    file_name.push(format!(".tmp-{}-{attempt}", std::process::id()));
    parent.join(file_name)
}

fn create_temporary_file(path: &Path) -> anyhow::Result<(std::fs::File, PathBuf)> {
    for attempt in 0..MAX_TEMPORARY_FILE_ATTEMPTS {
        let temporary_path = temporary_path(path, attempt);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((file, temporary_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }

    Err(anyhow::anyhow!(
        "could not create a unique temporary learning cache file after {MAX_TEMPORARY_FILE_ATTEMPTS} attempts"
    ))
}

/// Compute a candidate score: recency-weighted with frequency bonus.
///
/// Inspired by mozc's UserHistoryPredictor: recent selections rank higher,
/// with a logarithmic frequency term to reward repeated use.
fn score(entry: &LearningEntry, now: u64) -> f64 {
    let age_days = whole_age_days(now, entry.last_access);
    let recency = 1.0 / (1.0 + age_days as f64);
    let freq = (entry.frequency as f64).ln_1p();
    recency * 10.0 + freq
}

/// Compute elapsed whole days without underflow when a timestamp is ahead.
fn whole_age_days(now: u64, last_access: u64) -> u64 {
    now.saturating_sub(last_access) / 86400
}

/// Current time as Unix timestamp in seconds.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Config/cache with a custom entry limit and the default surface cap.
    fn config_with(max_entries: usize) -> LearningConfig {
        LearningConfig {
            max_entries,
            ..LearningConfig::default()
        }
    }

    fn cache_with(max_entries: usize) -> LearningCache {
        LearningCache::new(config_with(max_entries))
    }

    fn cache_with_ttl(max_entries: usize, singleton_ttl_days: u64) -> LearningCache {
        LearningCache::new(LearningConfig {
            singleton_ttl_days,
            ..config_with(max_entries)
        })
    }

    fn entry(surface: &str, frequency: u32, last_access: u64) -> LearningEntry {
        LearningEntry {
            surface: surface.to_string(),
            frequency,
            last_access,
        }
    }

    use tempfile::NamedTempFile;

    const DAY: u64 = 86_400;

    #[test]
    fn test_record_and_lookup() {
        let mut cache = cache_with(100);

        cache.record("きょう", "今日");
        cache.record("きょう", "京");
        cache.record("きょう", "今日"); // frequency bump

        let results = cache.lookup("きょう");
        assert_eq!(results.len(), 2);
        // "今日" should have higher score (frequency 2 vs 1)
        assert_eq!(results[0].0, "今日");
        assert_eq!(results[1].0, "京");
    }

    #[test]
    fn test_lookup_empty() {
        let cache = cache_with(100);
        let results = cache.lookup("きょう");
        assert!(results.is_empty());
    }

    #[test]
    fn lookup_min_frequency_filters_singletons_and_preserves_score_order() {
        let now = now_unix();
        let mut cache = cache_with(100);
        cache.entries.insert(
            "reading".to_string(),
            vec![
                entry("one-use", 1, now),
                entry("confirmed-old", 3, now.saturating_sub(DAY)),
                entry("confirmed-new", 2, now),
            ],
        );

        let expected: Vec<String> = cache
            .lookup("reading")
            .into_iter()
            .filter(|(surface, _)| surface != "one-use")
            .map(|(surface, _)| surface)
            .collect();
        let actual: Vec<String> = cache
            .lookup_min_frequency("reading", 2)
            .into_iter()
            .map(|(surface, _)| surface)
            .collect();

        assert_eq!(actual, expected);
        assert!(cache.lookup_min_frequency("missing", 2).is_empty());
    }

    #[test]
    fn test_prefix_lookup() {
        let mut cache = cache_with(100);
        cache.record("きょう", "今日");
        cache.record("きょうと", "京都");
        cache.record("あした", "明日");

        let results = cache.prefix_lookup("きょう");
        assert_eq!(results.len(), 2);
        // Both "きょう" and "きょうと" should match
        let readings: Vec<&str> = results.iter().map(|(r, _, _)| r.as_str()).collect();
        assert!(readings.contains(&"きょう"));
        assert!(readings.contains(&"きょうと"));
    }

    #[test]
    fn test_prefix_lookup_no_match() {
        let mut cache = cache_with(100);
        cache.record("きょう", "今日");
        let results = cache.prefix_lookup("あ");
        assert!(results.is_empty());
    }

    #[test]
    fn test_save_and_load() {
        let mut cache = cache_with(100);
        cache.record("きょう", "今日");
        cache.record("きょう", "今日");
        cache.record("きょう", "京");
        cache.record("あした", "明日");

        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();

        cache.save(&path).unwrap();
        assert!(!cache.is_dirty());

        let loaded = LearningCache::load(&path, config_with(100)).unwrap();
        assert!(!loaded.is_dirty());
        assert_eq!(loaded.entry_count(), 3);

        let results = loaded.lookup("きょう");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "今日"); // frequency 2
    }

    #[test]
    fn test_dirty_flag() {
        let mut cache = cache_with(100);
        assert!(!cache.is_dirty());

        cache.record("きょう", "今日");
        assert!(cache.is_dirty());

        let file = NamedTempFile::new().unwrap();
        cache.save(file.path()).unwrap();
        assert!(!cache.is_dirty());
    }

    #[test]
    fn singleton_older_than_ttl_is_pruned_on_save() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(100, 7);
        cache
            .entries
            .insert("old".to_string(), vec![entry("old", 1, now - 8 * DAY)]);
        cache.entries.insert(
            "recent".to_string(),
            vec![entry("recent", 1, now - 7 * DAY)],
        );
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(!cache.entries.contains_key("old"));
        assert!(cache.entries.contains_key("recent"));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn singleton_exactly_ttl_days_old_is_retained() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(100, 7);
        cache.entries.insert(
            "boundary".to_string(),
            vec![entry("boundary", 1, now - 7 * DAY)],
        );
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(cache.entries.contains_key("boundary"));
    }

    #[test]
    fn singleton_one_second_past_ttl_is_pruned() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(100, 7);
        cache.entries.insert(
            "past-boundary".to_string(),
            vec![entry("past-boundary", 1, now - 7 * DAY - 1)],
        );
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(!cache.entries.contains_key("past-boundary"));
    }

    #[test]
    fn ttl_handles_future_timestamps_and_large_values() {
        let now = u64::MAX - 1;
        let mut cache = cache_with_ttl(100, u64::MAX);
        cache
            .entries
            .insert("future".to_string(), vec![entry("future", 1, now + 1)]);
        cache
            .entries
            .insert("old".to_string(), vec![entry("old", 1, 0)]);
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(cache.entries.contains_key("future"));
        assert!(cache.entries.contains_key("old"));
    }

    #[test]
    fn old_repeated_entries_ignore_singleton_ttl() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(100, 7);
        cache.entries.insert(
            "repeated".to_string(),
            vec![entry("repeated", 2, now - 365 * DAY)],
        );
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(cache.entries.contains_key("repeated"));
    }

    #[test]
    fn zero_ttl_disables_singleton_pruning() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(100, 0);
        cache
            .entries
            .insert("old".to_string(), vec![entry("old", 1, now - 365 * DAY)]);
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert!(cache.entries.contains_key("old"));
    }

    #[test]
    fn ttl_prunes_before_capacity_and_removes_empty_readings() {
        let now = 1_000_000_000;
        let mut cache = cache_with_ttl(2, 7);
        cache.entries.insert(
            "expired".to_string(),
            vec![entry("expired", 1, now - 8 * DAY)],
        );
        cache.entries.insert(
            "repeated".to_string(),
            vec![entry("repeated", 2, now - 1_000 * DAY)],
        );
        cache
            .entries
            .insert("recent".to_string(), vec![entry("recent", 1, now)]);
        cache.dirty = true;

        let file = NamedTempFile::new().unwrap();
        cache.save_at(file.path(), now).unwrap();

        assert_eq!(cache.entry_count(), 2);
        assert!(!cache.entries.contains_key("expired"));
        assert!(cache.entries.contains_key("repeated"));
        assert!(cache.entries.contains_key("recent"));
    }

    #[test]
    fn ttl_replacement_failure_preserves_live_cache_and_destination() {
        let now = 1_000_000_000;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("learning.tsv");
        let original = b"# prior destination\nprior\tbytes\t1\t2\n";
        std::fs::write(&path, original).unwrap();

        let mut cache = cache_with_ttl(100, 7);
        cache.entries.insert(
            "expired".to_string(),
            vec![entry("expired", 1, now - 8 * DAY)],
        );
        cache.entries.insert(
            "repeated".to_string(),
            vec![entry("repeated", 2, now - 365 * DAY)],
        );
        cache.dirty = true;

        let result = cache.save_with_rename_at(&path, now, |_temporary, _destination| {
            Err(std::io::Error::other("deterministic rename failure"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(cache.entry_count(), 2);
        assert!(cache.entries.contains_key("expired"));
        assert!(cache.entries.contains_key("repeated"));
        assert!(cache.is_dirty());
    }

    #[test]
    fn test_eviction() {
        let mut cache = cache_with(3);

        // Add 5 entries
        cache.record("a", "A");
        cache.record("b", "B");
        cache.record("c", "C");
        cache.record("d", "D");
        cache.record("e", "E");

        // Boost some to give them higher scores
        cache.record("a", "A");
        cache.record("a", "A");
        cache.record("c", "C");

        let file = NamedTempFile::new().unwrap();
        cache.save(file.path()).unwrap();

        // After eviction, should be at most 3 entries
        assert!(cache.entry_count() <= 3);
    }

    #[test]
    fn test_score_recency() {
        let now = now_unix();
        let recent = LearningEntry {
            surface: "A".to_string(),
            frequency: 1,
            last_access: now,
        };
        let old = LearningEntry {
            surface: "B".to_string(),
            frequency: 1,
            last_access: now.saturating_sub(30 * 86400), // 30 days ago
        };
        assert!(score(&recent, now) > score(&old, now));
    }

    #[test]
    fn test_score_frequency() {
        let now = now_unix();
        let high_freq = LearningEntry {
            surface: "A".to_string(),
            frequency: 100,
            last_access: now,
        };
        let low_freq = LearningEntry {
            surface: "B".to_string(),
            frequency: 1,
            last_access: now,
        };
        assert!(score(&high_freq, now) > score(&low_freq, now));
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = LearningCache::load(Path::new("/nonexistent/path"), config_with(100));
        assert!(result.is_err());
    }

    #[test]
    fn test_tsv_format() {
        let mut cache = cache_with(100);
        cache.record("きょう", "今日");

        let file = NamedTempFile::new().unwrap();
        cache.save(file.path()).unwrap();

        let content = std::fs::read_to_string(file.path()).unwrap();
        assert!(content.starts_with("# karukan learning cache v1"));
        assert!(content.contains("きょう\t今日\t1\t"));
    }

    #[test]
    fn test_tsv_comments_and_blanks_ignored() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "# comment\n\nきょう\t今日\t5\t1700000000\n# another comment\n",
        )
        .unwrap();

        let cache = LearningCache::load(file.path(), config_with(100)).unwrap();
        assert_eq!(cache.entry_count(), 1);
        let results = cache.lookup("きょう");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "今日");
    }

    #[test]
    fn test_remove_suggestion_last_surface_drops_reading() {
        let mut cache = cache_with(100);
        cache.record("きょう", "今日");

        assert!(cache.remove_suggestion("きょう", "今日"));
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.lookup("きょう").is_empty());
        assert!(cache.prefix_lookup("き").is_empty());
    }

    #[test]
    fn test_remove_suggestion_clears_exact_and_prefix_twins() {
        let mut cache = cache_with(100);
        cache.record("あい", "藍");
        cache.record("あいさ", "藍"); // same surface, longer reading (a twin)
        cache.record("あい", "愛"); // different surface under the same reading
        cache.record("うみ", "藍"); // same surface, unrelated reading

        let file = NamedTempFile::new().unwrap();
        cache.save(file.path()).unwrap();
        assert!(!cache.is_dirty());

        assert!(cache.remove_suggestion("あい", "藍"));
        assert!(cache.is_dirty(), "removal must mark the cache dirty");

        // Both the exact entry and the prefix twin are gone...
        assert!(cache.lookup("あい").iter().all(|(s, _)| s != "藍"));
        assert!(cache.lookup("あいさ").is_empty());
        // ...but a different surface under the same reading survives...
        assert!(cache.lookup("あい").iter().any(|(s, _)| s == "愛"));
        // ...and the same surface under an unrelated reading is untouched.
        assert!(cache.lookup("うみ").iter().any(|(s, _)| s == "藍"));
    }

    #[test]
    fn test_remove_suggestion_nonexistent_is_noop() {
        let mut cache = cache_with(100);
        cache.record("あい", "藍");

        let file = NamedTempFile::new().unwrap();
        cache.save(file.path()).unwrap();

        assert!(!cache.remove_suggestion("あい", "愛"));
        assert!(!cache.remove_suggestion("かき", "柿"));
        assert!(!cache.is_dirty(), "no-op removal must not mark dirty");
    }

    #[test]
    fn test_record_skips_long_surface() {
        let mut cache = LearningCache::new(LearningConfig {
            max_entries: 100,
            max_surface_chars: 5,
            ..LearningConfig::default()
        });

        cache.record("あ", &"漢".repeat(6));
        assert_eq!(cache.entry_count(), 0);
        assert!(!cache.is_dirty());

        // Boundary: exactly max_surface_chars is accepted.
        cache.record("あ", &"漢".repeat(5));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_record_ignores_reading_length() {
        let mut cache = LearningCache::new(LearningConfig {
            max_entries: 100,
            max_surface_chars: 5,
            ..LearningConfig::default()
        });

        // Only the surface is capped; a long reading with a short surface
        // is fine (e.g. a long kana reading converting to a short word).
        cache.record(&"あ".repeat(30), "短い");
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_default_max_surface_chars() {
        let mut cache = cache_with(100);

        cache.record(
            "よみ",
            &"あ".repeat(LearningConfig::DEFAULT_MAX_SURFACE_CHARS + 1),
        );
        assert_eq!(cache.entry_count(), 0);

        cache.record(
            "よみ",
            &"あ".repeat(LearningConfig::DEFAULT_MAX_SURFACE_CHARS),
        );
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn test_tsv_malformed_lines_skipped() {
        let file = NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "きょう\t今日\t5\t1700000000\nmalformed_line\nきょう\t京\tbad\t1700000000\n",
        )
        .unwrap();

        let cache = LearningCache::load(file.path(), config_with(100)).unwrap();
        // Only the first valid line should be loaded
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn red_atomic_learning_save_keeps_original_on_replacement_error() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("learning.tsv");
        let original = b"# prior destination\nprior\tbytes\t1\t2\n";
        std::fs::write(&path, original).unwrap();

        let mut cache = cache_with(1);
        cache.record("あい", "藍");
        cache.record("うえ", "上");
        assert!(cache.is_dirty());
        let result = cache.save_with_rename(&path, |_temporary, _destination| {
            Err(std::io::Error::other("deterministic rename failure"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(cache.lookup("あい")[0].0, "藍");
        assert_eq!(cache.lookup("うえ")[0].0, "上");
        assert!(cache.is_dirty());

        let temporary_files: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "learning.tsv")
            .collect();
        assert!(
            temporary_files.is_empty(),
            "temporary files left behind: {temporary_files:?}"
        );
    }

    #[test]
    fn relative_destination_normalizes_empty_parent_to_current_directory() {
        assert_eq!(
            normalized_parent_dir(Path::new("learning.tsv")),
            Path::new(".")
        );
        assert_eq!(
            normalized_parent_dir(Path::new("cache/learning.tsv")),
            Path::new("cache")
        );
    }

    #[test]
    fn save_advances_when_first_temporary_path_exists() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("learning.tsv");
        let occupied_path = temporary_path(&path, 0);
        let marker = b"must not be overwritten";
        std::fs::write(&occupied_path, marker).unwrap();

        let mut cache = cache_with(100);
        cache.record("あい", "藍");
        cache.save(&path).unwrap();

        assert_eq!(std::fs::read(&occupied_path).unwrap(), marker);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("あい\t藍\t1\t")
        );
    }

    #[test]
    fn all_temporary_path_collisions_preserve_destination_and_live_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("learning.tsv");
        let original = b"# prior destination\nprior\tbytes\t1\t2\n";
        std::fs::write(&path, original).unwrap();

        let markers: Vec<Vec<u8>> = (0..MAX_TEMPORARY_FILE_ATTEMPTS)
            .map(|attempt| {
                let marker = format!("occupied temporary path {attempt}").into_bytes();
                std::fs::write(temporary_path(&path, attempt), &marker).unwrap();
                marker
            })
            .collect();

        let mut cache = cache_with(1);
        cache.record("あい", "藍");
        cache.record("うえ", "上");
        assert!(cache.is_dirty());

        let result = cache.save(&path);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(cache.lookup("あい")[0].0, "藍");
        assert_eq!(cache.lookup("うえ")[0].0, "上");
        assert!(cache.is_dirty());
        for (attempt, marker) in markers.iter().enumerate() {
            assert_eq!(
                std::fs::read(temporary_path(&path, attempt)).unwrap(),
                *marker
            );
        }
    }
}
