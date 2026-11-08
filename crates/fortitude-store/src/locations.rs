//! Cache of pixel data locations (file, transfer syntax, frame layout), by series.
//!
//! A viewer opening a series requests its frames one instance at a time, and each request
//! would otherwise query the index: with a remote database, a network round trip per frame.
//! The first request of a series loads the locations of all its instances in one query,
//! shared by the requests arriving meanwhile; the following ones are answered from memory.
//!
//! Stored instances never change, so an entry only goes stale when instances are deleted.
//! Deletions through this store evict the series they touch; entries also expire a fixed
//! time after they were loaded, which bounds staleness when another process deletes from a
//! shared index. Instances stored after their series was loaded are looked up one by one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

use crate::retrieve::{PixelSource, Scope};

/// Study and series UIDs.
type SeriesKey = (String, String);

/// Locations of the instances of a series, by SOP Instance UID.
pub(crate) type SeriesLocations = HashMap<String, Arc<PixelSource>>;

pub(crate) struct LocationCache {
    /// Maximum total weight of the cached series, in bytes; 0 disables the cache.
    capacity: usize,
    ttl: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    series: HashMap<SeriesKey, Cached>,
    /// Loads in progress, joined by the requests for the same series.
    loading: HashMap<SeriesKey, Load>,
    weight: usize,
    /// Incremented by each deletion: a load started before it may hold deleted instances,
    /// and is not cached.
    generation: u64,
}

struct Cached {
    locations: Arc<SeriesLocations>,
    loaded_at: Instant,
    used_at: Instant,
    weight: usize,
}

/// A load of the locations of a series: `None` when it failed.
#[derive(Clone)]
pub(crate) struct Load {
    pub(crate) cell: Arc<OnceCell<Option<Arc<SeriesLocations>>>>,
    generation: u64,
}

pub(crate) enum Lookup {
    Found(Arc<PixelSource>),
    /// The series is cached without this instance: stored since, or not stored.
    Absent,
    /// The series is not cached: run or join its load, then [finish](LocationCache::finish) it.
    Load(Load),
}

/// Approximate memory used by one cached instance, besides its strings and layout.
const ENTRY_OVERHEAD: usize = size_of::<PixelSource>() + 64;

impl LocationCache {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            state: Mutex::default(),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.capacity > 0 && !self.ttl.is_zero()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn lookup(&self, study: &str, series: &str, instance: &str) -> Lookup {
        let key = (study.to_owned(), series.to_owned());
        let now = Instant::now();
        let mut state = self.state();
        if let Some(cached) = state.series.get_mut(&key) {
            if now.duration_since(cached.loaded_at) < self.ttl {
                cached.used_at = now;
                return match cached.locations.get(instance) {
                    Some(source) => Lookup::Found(source.clone()),
                    None => Lookup::Absent,
                };
            }
            if let Some(expired) = state.series.remove(&key) {
                state.weight -= expired.weight;
            }
        }
        let generation = state.generation;
        let load = state.loading.entry(key).or_insert_with(|| Load {
            cell: Arc::new(OnceCell::new()),
            generation,
        });
        Lookup::Load(load.clone())
    }

    /// End `load` of a series: its result is cached unless the load failed, found no
    /// instance, or a deletion happened since it started. Every request that joined the load
    /// may call it; only the first has an effect.
    pub(crate) fn finish(&self, study: &str, series: &str, load: &Load) {
        let key = (study.to_owned(), series.to_owned());
        let mut state = self.state();
        if !state
            .loading
            .get(&key)
            .is_some_and(|l| Arc::ptr_eq(&l.cell, &load.cell))
        {
            return;
        }
        state.loading.remove(&key);
        let Some(Some(locations)) = load.cell.get() else {
            return;
        };
        let weight = weight(locations);
        if locations.is_empty() || load.generation != state.generation || weight > self.capacity {
            return;
        }
        let now = Instant::now();
        state.weight += weight;
        if let Some(replaced) = state.series.insert(
            key,
            Cached {
                locations: locations.clone(),
                loaded_at: now,
                used_at: now,
                weight,
            },
        ) {
            state.weight -= replaced.weight;
        }
        // Evict expired series, then the least recently used ones.
        let ttl = self.ttl;
        let State { series, weight, .. } = &mut *state;
        series.retain(|_, c| {
            let keep = now.duration_since(c.loaded_at) < ttl;
            if !keep {
                *weight -= c.weight;
            }
            keep
        });
        while state.weight > self.capacity {
            let Some(oldest) = state
                .series
                .iter()
                .min_by_key(|(_, c)| c.used_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(evicted) = state.series.remove(&oldest) {
                state.weight -= evicted.weight;
            }
        }
    }

    /// Evict the series of `scope`, whose instances were deleted.
    pub(crate) fn invalidate(&self, scope: &Scope) {
        let mut state = self.state();
        state.generation += 1;
        let State { series, weight, .. } = &mut *state;
        series.retain(|(study, series), c| {
            let keep = study != &scope.study || scope.series.as_ref().is_some_and(|s| s != series);
            if !keep {
                *weight -= c.weight;
            }
            keep
        });
    }
}

fn weight(locations: &SeriesLocations) -> usize {
    locations
        .iter()
        .map(|(sop, source)| {
            ENTRY_OVERHEAD
                + sop.len()
                + source.path.len()
                + source.transfer_syntax.len()
                + source.layout.as_ref().map_or(0, |l| l.heap_size())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(path: &str) -> Arc<PixelSource> {
        Arc::new(PixelSource {
            path: path.into(),
            transfer_syntax: "1.2.840.10008.1.2.1".into(),
            layout: None,
        })
    }

    fn series(instances: &[&str]) -> Option<Arc<SeriesLocations>> {
        Some(Arc::new(
            instances
                .iter()
                .map(|sop| (sop.to_string(), source(&format!("files/{sop}"))))
                .collect(),
        ))
    }

    /// Look up `instance`, loading `loaded` when the series is not cached.
    fn get(
        cache: &LocationCache,
        series_uid: &str,
        instance: &str,
        loaded: Option<Arc<SeriesLocations>>,
    ) -> Option<String> {
        match cache.lookup("study", series_uid, instance) {
            Lookup::Found(source) => Some(source.path.clone()),
            Lookup::Absent => None,
            Lookup::Load(load) => {
                load.cell.set(loaded).ok();
                cache.finish("study", series_uid, &load);
                load.cell
                    .get()
                    .cloned()
                    .flatten()
                    .and_then(|l| l.get(instance).map(|s| s.path.clone()))
            }
        }
    }

    fn cached(cache: &LocationCache, series_uid: &str, instance: &str) -> bool {
        matches!(
            cache.lookup("study", series_uid, instance),
            Lookup::Found(_)
        )
    }

    #[test]
    fn serves_a_loaded_series() {
        let cache = LocationCache::new(1 << 20, Duration::from_secs(60));
        assert!(!cached(&cache, "s1", "a"));
        get(&cache, "s1", "a", series(&["a", "b"]));
        assert!(cached(&cache, "s1", "b"));
        // An instance missing from the cached series is looked up individually.
        assert!(matches!(cache.lookup("study", "s1", "c"), Lookup::Absent));
    }

    #[test]
    fn concurrent_requests_join_one_load() {
        let cache = LocationCache::new(1 << 20, Duration::from_secs(60));
        let (Lookup::Load(first), Lookup::Load(second)) = (
            cache.lookup("study", "s1", "a"),
            cache.lookup("study", "s1", "b"),
        ) else {
            panic!("expected loads");
        };
        assert!(Arc::ptr_eq(&first.cell, &second.cell));
        first.cell.set(series(&["a", "b"])).ok();
        cache.finish("study", "s1", &first);
        cache.finish("study", "s1", &second);
        assert!(cached(&cache, "s1", "a"));
    }

    #[test]
    fn failed_or_empty_loads_are_not_cached() {
        let cache = LocationCache::new(1 << 20, Duration::from_secs(60));
        assert_eq!(get(&cache, "s1", "a", None), None);
        assert!(matches!(cache.lookup("study", "s1", "a"), Lookup::Load(_)));
        let cache = LocationCache::new(1 << 20, Duration::from_secs(60));
        assert_eq!(get(&cache, "s2", "a", series(&[])), None);
        assert!(matches!(cache.lookup("study", "s2", "a"), Lookup::Load(_)));
    }

    #[test]
    fn deletions_evict_and_void_loads_in_progress() {
        let cache = LocationCache::new(1 << 20, Duration::from_secs(60));
        get(&cache, "s1", "a", series(&["a"]));
        get(&cache, "s2", "b", series(&["b"]));
        cache.invalidate(&Scope::instance("study", "s1", "a"));
        assert!(!cached(&cache, "s1", "a"));
        assert!(cached(&cache, "s2", "b"));

        // A load started before a deletion may have read the deleted instances.
        let Lookup::Load(load) = cache.lookup("study", "s3", "c") else {
            panic!("expected a load");
        };
        cache.invalidate(&Scope::study("study"));
        assert!(!cached(&cache, "s2", "b"));
        load.cell.set(series(&["c"])).ok();
        cache.finish("study", "s3", &load);
        assert!(!cached(&cache, "s3", "c"));

        get(&cache, "s4", "d", series(&["d"]));
        cache.invalidate(&Scope::study("other"));
        assert!(cached(&cache, "s4", "d"));
    }

    #[test]
    fn evicts_least_recently_used_series() {
        let one = weight(series(&["a"]).as_deref().unwrap());
        let cache = LocationCache::new(2 * one, Duration::from_secs(60));
        get(&cache, "s1", "a", series(&["a"]));
        get(&cache, "s2", "a", series(&["a"]));
        assert!(cached(&cache, "s1", "a"));
        get(&cache, "s3", "a", series(&["a"]));
        assert!(cached(&cache, "s1", "a"));
        assert!(cached(&cache, "s3", "a"));
        assert!(!cached(&cache, "s2", "a"));
        // A series larger than the whole cache is not kept.
        get(&cache, "s4", "a", series(&["a", "b", "c"]));
        assert!(!cached(&cache, "s4", "a"));
    }

    #[test]
    fn entries_expire() {
        let cache = LocationCache::new(1 << 20, Duration::from_millis(20));
        get(&cache, "s1", "a", series(&["a"]));
        assert!(cached(&cache, "s1", "a"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!cached(&cache, "s1", "a"));
    }
}
