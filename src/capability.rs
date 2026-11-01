//! Persistent, chain-agnostic record of how much historical state each
//! endpoint can serve correctly.
//!
//! Depths are stored as `(block, head_at_observation)` pairs so that both
//! archive nodes (absolute: "serves block 12M") and windowed nodes (relative:
//! "serves the last ~128 blocks") are described correctly as the head moves.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a capability observation (including the liar flag) is trusted.
pub const STATE_CAPABILITY_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What the cache knows about one endpoint for one chain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCapability {
    pub chain_id: String,
    pub url: String,
    /// Deepest (lowest) block whose state the endpoint was verified to serve.
    pub verified_block: Option<u64>,
    /// Chain head observed when `verified_block` was verified.
    pub verified_head: Option<u64>,
    /// Shallowest (highest) block at which the endpoint reported pruned state.
    pub pruned_block: Option<u64>,
    /// Chain head observed when `pruned_block` was seen.
    pub pruned_head: Option<u64>,
    /// Returned well-formed but wrong state (failed the nonce check).
    pub liar: bool,
    /// Average per-call latency measured by the last probe.
    pub latency_ms: Option<u64>,
    /// Unix seconds after which this record is ignored.
    pub expires_at: u64,
}

/// Whether an endpoint can serve state at a given block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Verified to serve state at this depth.
    Covered,
    /// No fresh information.
    Unknown,
    /// Reported pruned state at this depth or shallower.
    Pruned,
    /// Caught returning wrong data. Never route to it.
    Liar,
}

fn depth(block: u64, head: Option<u64>) -> Option<u64> {
    head.map(|h| h.saturating_sub(block))
}

impl StateCapability {
    fn verified_depth(&self) -> Option<u64> {
        depth(self.verified_block?, self.verified_head)
    }

    fn pruned_depth(&self) -> Option<u64> {
        depth(self.pruned_block?, self.pruned_head)
    }

    /// `block = None` means "latest". `head` is the current chain head, if known;
    /// without it, comparisons fall back to absolute block numbers.
    pub fn coverage(&self, block: Option<u64>, head: Option<u64>) -> Coverage {
        if self.liar {
            return Coverage::Liar;
        }
        let Some(block) = block else {
            return if self.verified_block.is_some() {
                Coverage::Covered
            } else {
                Coverage::Unknown
            };
        };
        let request_depth = depth(block, head);

        if let Some(pruned) = self.pruned_block {
            let is_pruned = match (request_depth, self.pruned_depth()) {
                (Some(want), Some(pruned_depth)) => want >= pruned_depth,
                _ => block <= pruned,
            };
            if is_pruned {
                return Coverage::Pruned;
            }
        }
        if let Some(verified) = self.verified_block {
            // A verification at depth d keeps covering blocks at or after the
            // verified block until they are 2*d deep. Archive nodes verified long
            // ago stay covered as the head advances; a windowed node verified
            // near the tip ages out (and gets re-probed) within a few blocks.
            let covered = match (request_depth, self.verified_depth()) {
                (Some(want), Some(verified_depth)) => {
                    want <= verified_depth
                        || (block >= verified && want <= verified_depth.saturating_mul(2))
                }
                _ => block >= verified,
            };
            if covered {
                return Coverage::Covered;
            }
        }
        Coverage::Unknown
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedStateFile {
    endpoints: HashMap<String, StateCapability>,
}

/// Thread-safe capability cache, optionally persisted as JSON.
#[derive(Debug, Clone)]
pub struct StateCapabilityStore {
    path: Option<PathBuf>,
    entries: Arc<Mutex<HashMap<String, StateCapability>>>,
    /// Highest head seen per chain (memory only).
    heads: Arc<Mutex<HashMap<String, u64>>>,
    /// Serializes snapshot + write + rename so concurrent updates cannot
    /// interleave or land out of order.
    write_lock: Arc<Mutex<()>>,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn key(chain_id: &str, url: &str) -> String {
    format!("{chain_id}\n{url}")
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl StateCapabilityStore {
    /// Memory-only store.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            entries: Arc::new(Mutex::new(HashMap::new())),
            heads: Arc::new(Mutex::new(HashMap::new())),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Load from `path` (missing or corrupt files start empty); expired records are dropped.
    pub fn load(path: PathBuf) -> Self {
        Self::load_at(path, unix_now())
    }

    fn load_at(path: PathBuf, now: u64) -> Self {
        let entries = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<PersistedStateFile>(&s).ok())
            .map(|file| {
                file.endpoints
                    .into_iter()
                    .filter(|(_, entry)| entry.expires_at > now)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: Arc::new(Mutex::new(entries)),
            heads: Arc::new(Mutex::new(HashMap::new())),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Fresh record for an endpoint, if any.
    pub fn get(&self, chain_id: &str, url: &str) -> Option<StateCapability> {
        self.get_at(chain_id, url, unix_now())
    }

    fn get_at(&self, chain_id: &str, url: &str, now: u64) -> Option<StateCapability> {
        let entries = self.entries.lock().unwrap();
        entries
            .get(&key(chain_id, url))
            .filter(|entry| entry.expires_at > now)
            .cloned()
    }

    pub fn coverage(&self, chain_id: &str, url: &str, block: Option<u64>) -> Coverage {
        let head = self.head(chain_id);
        self.get(chain_id, url)
            .map(|c| c.coverage(block, head))
            .unwrap_or(Coverage::Unknown)
    }

    pub fn is_liar(&self, chain_id: &str, url: &str) -> bool {
        self.get(chain_id, url).is_some_and(|c| c.liar)
    }

    pub fn note_head(&self, chain_id: &str, head: u64) {
        let mut heads = self.heads.lock().unwrap();
        let entry = heads.entry(chain_id.to_string()).or_insert(0);
        if head > *entry {
            *entry = head;
        }
    }

    /// Best known head for a chain: live observations first, then cached records.
    pub fn head(&self, chain_id: &str) -> Option<u64> {
        if let Some(head) = self.heads.lock().unwrap().get(chain_id) {
            return Some(*head);
        }
        let entries = self.entries.lock().unwrap();
        entries
            .values()
            .filter(|e| e.chain_id == chain_id)
            .flat_map(|e| [e.verified_head, e.pruned_head])
            .flatten()
            .max()
    }

    fn update<F: FnOnce(&mut StateCapability)>(&self, chain_id: &str, url: &str, now: u64, f: F) {
        let changed = {
            let mut entries = self.entries.lock().unwrap();
            let entry = entries
                .entry(key(chain_id, url))
                .or_insert_with(|| StateCapability {
                    chain_id: chain_id.to_string(),
                    url: url.to_string(),
                    ..Default::default()
                });
            if entry.expires_at <= now {
                *entry = StateCapability {
                    chain_id: chain_id.to_string(),
                    url: url.to_string(),
                    ..Default::default()
                };
            }
            let before = entry.clone();
            f(entry);
            entry.expires_at = now + STATE_CAPABILITY_TTL.as_secs();
            // Refreshing only the expiry of an unchanged record is not worth a write
            // unless it is getting old.
            let mut previous = before;
            let stale = previous.expires_at < now + STATE_CAPABILITY_TTL.as_secs() / 2;
            previous.expires_at = entry.expires_at;
            previous != *entry || stale
        };
        if changed {
            self.persist();
        }
    }

    /// The endpoint served correct state at `block` while the head was `head`.
    pub fn record_verified(
        &self,
        chain_id: &str,
        url: &str,
        block: u64,
        head: Option<u64>,
        latency_ms: Option<u64>,
    ) {
        self.record_verified_at(chain_id, url, block, head, latency_ms, unix_now());
    }

    fn record_verified_at(
        &self,
        chain_id: &str,
        url: &str,
        block: u64,
        head: Option<u64>,
        latency_ms: Option<u64>,
        now: u64,
    ) {
        if let Some(head) = head {
            self.note_head(chain_id, head);
        }
        self.update(chain_id, url, now, |entry| {
            let new_depth = depth(block, head);
            let deeper = match (entry.verified_block, new_depth, entry.verified_depth()) {
                (None, _, _) => true,
                (Some(_), Some(new), Some(old)) => new >= old,
                (Some(old_block), _, _) => block <= old_block,
            };
            if deeper {
                entry.verified_block = Some(block);
                entry.verified_head = head;
            }
            // A verified depth at or beyond the pruned mark invalidates it.
            if entry.pruned_block.is_some() {
                let clears = match (new_depth, entry.pruned_depth()) {
                    (Some(new), Some(pruned)) => new >= pruned,
                    _ => entry.pruned_block.is_some_and(|p| block <= p),
                };
                if clears {
                    entry.pruned_block = None;
                    entry.pruned_head = None;
                }
            }
            if latency_ms.is_some() {
                entry.latency_ms = latency_ms;
            }
        });
    }

    /// The endpoint reported missing/pruned state at `block`.
    pub fn record_pruned(&self, chain_id: &str, url: &str, block: u64, head: Option<u64>) {
        self.record_pruned_at(chain_id, url, block, head, unix_now());
    }

    fn record_pruned_at(&self, chain_id: &str, url: &str, block: u64, head: Option<u64>, now: u64) {
        self.update(chain_id, url, now, |entry| {
            let new_depth = depth(block, head);
            let shallower = match (entry.pruned_block, new_depth, entry.pruned_depth()) {
                (None, _, _) => true,
                (Some(_), Some(new), Some(old)) => new <= old,
                (Some(old_block), _, _) => block >= old_block,
            };
            if shallower {
                entry.pruned_block = Some(block);
                entry.pruned_head = head;
            }
            // Contradicts an earlier verification at this depth (e.g. a load
            // balancer mixing archive and full nodes): stop trusting it.
            if entry.verified_block.is_some() {
                let contradicts = match (new_depth, entry.verified_depth()) {
                    (Some(new), Some(verified)) => verified >= new,
                    _ => entry.verified_block.is_some_and(|v| v <= block),
                };
                if contradicts {
                    entry.verified_block = None;
                    entry.verified_head = None;
                }
            }
        });
    }

    /// The endpoint returned well-formed but wrong state.
    pub fn record_liar(&self, chain_id: &str, url: &str) {
        self.record_liar_at(chain_id, url, unix_now());
    }

    fn record_liar_at(&self, chain_id: &str, url: &str, now: u64) {
        self.update(chain_id, url, now, |entry| {
            entry.liar = true;
            entry.verified_block = None;
            entry.verified_head = None;
        });
    }

    fn persist(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!(
                "failed to create state store directory {}: {}",
                parent.display(),
                err
            );
            return;
        }
        let _guard = self.write_lock.lock().unwrap();
        let now = unix_now();
        let file = PersistedStateFile {
            endpoints: self
                .entries
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, e)| e.expires_at > now)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        let contents = match serde_json::to_string_pretty(&file) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("failed to serialize state store: {err}");
                return;
            }
        };
        // Write-then-rename so concurrent readers never see a torn file.
        let tmp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(err) = fs::write(&tmp, contents).and_then(|_| fs::rename(&tmp, path)) {
            eprintln!("failed to write state store {}: {}", path.display(), err);
        }
    }
}

/// Default state store: `$SINGLERPC_STATE_FILE`, else a sibling of the health file
/// (`~/.singlerpc/health.state.json`). An empty `$SINGLERPC_STATE_FILE` disables persistence.
pub fn default_state_store_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SINGLERPC_STATE_FILE") {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(PathBuf::from(trimmed));
    }
    crate::proxy::default_health_store_path().map(|p| state_store_path_for_health_file(&p))
}

/// `<dir>/<stem>.state.json` next to a health file.
pub fn state_store_path_for_health_file(health_path: &Path) -> PathBuf {
    let stem = health_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "health".to_string());
    health_path.with_file_name(format!("{stem}.state.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("singlerpc-cap-{nanos}-{name}"))
    }

    const URL: &str = "https://node.example";

    #[test]
    fn records_expire_after_ttl() {
        let store = StateCapabilityStore::in_memory();
        let now = 1_000_000;
        store.record_liar_at("1", URL, now);
        assert!(store.get_at("1", URL, now + 10).unwrap().liar);
        let ttl = STATE_CAPABILITY_TTL.as_secs();
        assert!(store.get_at("1", URL, now + ttl - 1).is_some());
        assert!(store.get_at("1", URL, now + ttl).is_none());
    }

    #[test]
    fn expired_records_are_dropped_on_load_and_reset_on_update() {
        let path = temp_path("expire.json");
        let store = StateCapabilityStore::load(path.clone());
        let long_ago = unix_now() - STATE_CAPABILITY_TTL.as_secs() - 5;
        store.record_liar_at("1", URL, long_ago);
        store.record_verified("1", "https://other", 10, Some(20), Some(5));
        let reloaded = StateCapabilityStore::load(path.clone());
        assert!(reloaded.get("1", URL).is_none());
        assert!(reloaded.get("1", "https://other").is_some());
        // An update on top of an expired liar record starts fresh.
        store.record_verified("1", URL, 10, Some(20), None);
        assert!(!store.get("1", URL).unwrap().liar);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persistence_round_trip() {
        let path = temp_path("roundtrip.json");
        let store = StateCapabilityStore::load(path.clone());
        store.record_verified("1", URL, 12_000_000, Some(24_000_000), Some(42));
        store.record_pruned("8453", URL, 100, Some(1_000));
        store.record_liar("1", "https://liar.example");

        let reloaded = StateCapabilityStore::load(path.clone());
        let a = reloaded.get("1", URL).unwrap();
        assert_eq!(a.verified_block, Some(12_000_000));
        assert_eq!(a.verified_head, Some(24_000_000));
        assert_eq!(a.latency_ms, Some(42));
        let b = reloaded.get("8453", URL).unwrap();
        assert_eq!(b.pruned_block, Some(100));
        assert!(reloaded.is_liar("1", "https://liar.example"));
        assert_eq!(reloaded.head("1"), Some(24_000_000));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn liar_is_never_covered() {
        let store = StateCapabilityStore::in_memory();
        store.record_verified("1", URL, 0, Some(100), None);
        assert_eq!(store.coverage("1", URL, Some(50)), Coverage::Covered);
        store.record_liar("1", URL);
        assert_eq!(store.coverage("1", URL, Some(50)), Coverage::Liar);
        assert_eq!(store.coverage("1", URL, None), Coverage::Liar);
        // Later verifications cannot launder a liar within the TTL.
        store.record_verified("1", URL, 0, Some(100), None);
        assert_eq!(store.coverage("1", URL, Some(99)), Coverage::Liar);
    }

    #[test]
    fn depth_selection_follows_the_head() {
        let archive = StateCapability {
            verified_block: Some(12_000_000),
            verified_head: Some(24_000_000),
            ..Default::default()
        };
        let windowed = StateCapability {
            verified_block: Some(24_000_000 - 2),
            verified_head: Some(24_000_000),
            pruned_block: Some(24_000_000 - 1_000),
            pruned_head: Some(24_000_000),
            ..Default::default()
        };
        let head_later = Some(24_050_000);
        assert_eq!(
            archive.coverage(Some(12_000_000), head_later),
            Coverage::Covered
        );
        assert_eq!(
            archive.coverage(Some(11_000_000), head_later),
            Coverage::Unknown
        );
        // 50k blocks later the windowed node's absolute "verified" block is far
        // outside its window; depth-based checks catch that.
        assert_eq!(
            windowed.coverage(Some(24_000_000 - 2), head_later),
            Coverage::Pruned
        );
        assert_eq!(
            windowed.coverage(Some(24_049_999), head_later),
            Coverage::Covered
        );
        assert_eq!(
            windowed.coverage(Some(24_049_500), head_later),
            Coverage::Unknown
        );
        assert_eq!(windowed.coverage(None, head_later), Coverage::Covered);
        // Without a head estimate, absolute comparisons are used.
        assert_eq!(archive.coverage(Some(13_000_000), None), Coverage::Covered);
        assert_eq!(
            windowed.coverage(Some(24_000_000 - 5_000), None),
            Coverage::Pruned
        );
    }

    #[test]
    fn pruned_and_verified_marks_reconcile() {
        let store = StateCapabilityStore::in_memory();
        store.record_pruned("1", URL, 900, Some(1_000));
        assert_eq!(store.coverage("1", URL, Some(800)), Coverage::Pruned);
        // Verified deeper than the pruned mark: the pruned mark was wrong/stale.
        store.record_verified("1", URL, 500, Some(1_000), None);
        assert_eq!(store.coverage("1", URL, Some(800)), Coverage::Covered);
        // Then a contradicting pruned observation drops the verification.
        store.record_pruned("1", URL, 700, Some(1_000));
        assert_eq!(store.coverage("1", URL, Some(600)), Coverage::Pruned);
        assert_eq!(store.coverage("1", URL, Some(990)), Coverage::Unknown);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_updates_all_persist() {
        let path = temp_path("concurrent.json");
        let store = StateCapabilityStore::load(path.clone());
        let tasks: Vec<_> = (0..200u64)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move {
                    let url = format!("https://node-{i}.example");
                    if i % 3 == 0 {
                        store.record_liar("1", &url);
                    } else {
                        store.record_pruned("1", &url, i, Some(1_000));
                    }
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        let reloaded = StateCapabilityStore::load(path.clone());
        for i in 0..200u64 {
            let record = reloaded
                .get("1", &format!("https://node-{i}.example"))
                .unwrap_or_else(|| panic!("record {i} lost"));
            assert_eq!(record.liar, i % 3 == 0);
        }
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&*path.file_stem().unwrap().to_string_lossy())
                    && e.path() != path
            })
            .count();
        assert_eq!(leftovers, 0, "temp files left behind");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn health_sibling_path() {
        assert_eq!(
            state_store_path_for_health_file(Path::new("/x/.singlerpc/health.json")),
            PathBuf::from("/x/.singlerpc/health.state.json")
        );
    }
}
