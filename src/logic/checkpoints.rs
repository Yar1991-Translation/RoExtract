//! Cache Checkpoint (缓存关键帧): snapshots of the cache state used as
//! "time boundaries" for observing which caches appeared, changed or
//! disappeared afterwards.
//!
//! Checkpoints never delete or modify cache data; they only store the
//! identities and content fingerprints of the caches that existed when the
//! checkpoint was created. They are persisted through the existing config
//! mechanism (`RoExtract-config.json`) so they survive restarts.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::logic::{self, AssetInfo, Category};
use crate::{config, locale};

const CHECKPOINTS_KEY: &str = "cache_checkpoints";
const ACTIVE_CHECKPOINT_KEY: &str = "active_checkpoint";

/// One cache record inside a checkpoint snapshot.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CheckpointEntry {
    /// Stable unique cache identity (source + category + name).
    pub key: String,
    /// Raw cache name within its source.
    pub name: String,
    /// Size of the cached source data in bytes.
    pub size: u64,
    /// Last modification time (unix seconds) when available.
    pub modified_secs: Option<i64>,
    /// Content fingerprint (FNV-1a over the inspected source bytes).
    pub fingerprint: Option<u64>,
    /// Detected file format, e.g. "PNG", "WebP", "MP4" or "unknown".
    pub file_type: Option<String>,
    /// Detected file extension (no dot), e.g. "png".
    pub extension: Option<String>,
}

/// A snapshot of the cache state at a point in time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CacheCheckpoint {
    /// Unique identifier.
    pub id: String,
    /// User-visible name, defaults to "Checkpoint N".
    pub name: String,
    /// Creation time (unix seconds).
    pub created_at_secs: i64,
    /// The cache state at creation time.
    pub entries: Vec<CheckpointEntry>,
}

/// Result of comparing a checkpoint snapshot with the current cache state.
#[derive(Clone, Debug, Default)]
pub struct CheckpointDiff {
    /// Caches that exist now but did not exist at checkpoint time.
    pub added: Vec<AssetInfo>,
    /// Caches that exist in both states but differ (size, fingerprint, mtime).
    pub modified: Vec<AssetInfo>,
    /// Caches captured at checkpoint time that no longer exist now.
    pub removed: Vec<CheckpointEntry>,
}

static CHECKPOINTS: LazyLock<Mutex<Vec<CacheCheckpoint>>> =
    LazyLock::new(|| Mutex::new(load_checkpoints()));
static ACTIVE_CHECKPOINT: LazyLock<Mutex<Option<String>>> =
    LazyLock::new(|| Mutex::new(load_active_checkpoint()));
/// Latest full cache state captured by a checkpoint refresh, used by the
/// checkpoint UI to show change counts without re-reading the cache.
static CURRENT_CACHE: LazyLock<Mutex<Arc<Vec<AssetInfo>>>> =
    LazyLock::new(|| Mutex::new(Arc::new(Vec::new())));
/// Bumped every time `CURRENT_CACHE` is replaced.
static CURRENT_GENERATION: AtomicU64 = AtomicU64::new(0);

fn load_checkpoints() -> Vec<CacheCheckpoint> {
    config::get_config()
        .get(CHECKPOINTS_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or_default()
}

fn load_active_checkpoint() -> Option<String> {
    config::get_config_string(ACTIVE_CHECKPOINT_KEY)
}

fn persist_checkpoints() {
    let checkpoints = CHECKPOINTS.lock().unwrap().clone();
    match serde_json::to_value(&checkpoints) {
        Ok(value) => {
            config::set_config_value(CHECKPOINTS_KEY, value);
            config::save_config_file();
        }
        Err(e) => log_error!("Failed to serialize checkpoints: {}", e),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn new_checkpoint_id() -> String {
    format!(
        "checkpoint-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    )
}

fn system_time_secs(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Stable unique identity of a cache record across refresh runs.
///
/// Combines the source backend (cache-directory file / SQL row / rbx-storage
/// file), the category/directory the record lives in and its name, so changes
/// are never inferred from array positions or display order.
pub fn asset_key(asset: &AssetInfo) -> String {
    let source = if asset.from_sql {
        "sql"
    } else if asset.from_rbx_storage {
        "rbx"
    } else {
        "file"
    };
    format!("{source}:{}:{}", asset.category, asset.name)
}

fn checkpoint_entry(asset: &AssetInfo) -> CheckpointEntry {
    CheckpointEntry {
        key: asset_key(asset),
        name: asset.name.clone(),
        size: asset._size,
        modified_secs: asset.last_modified.map(system_time_secs),
        fingerprint: asset.fingerprint,
        file_type: asset.file_type.clone(),
        extension: asset.extension.clone(),
    }
}

/// Whether the current version of a cache differs from its snapshot record.
pub fn entry_version_changed(entry: &CheckpointEntry, asset: &AssetInfo) -> bool {
    entry.fingerprint != asset.fingerprint
        || entry.size != asset._size
        || entry.modified_secs != asset.last_modified.map(system_time_secs)
}

/// Compare a checkpoint snapshot with the current cache state.
pub fn diff_checkpoint(checkpoint: &CacheCheckpoint, current: &[AssetInfo]) -> CheckpointDiff {
    let mut by_key: HashMap<&str, &CheckpointEntry> = HashMap::with_capacity(checkpoint.entries.len());
    for entry in &checkpoint.entries {
        by_key.insert(entry.key.as_str(), entry);
    }

    let mut diff = CheckpointDiff::default();
    let mut current_keys: HashSet<String> = HashSet::with_capacity(current.len());

    for asset in current {
        let key = asset_key(asset);
        match by_key.get(key.as_str()) {
            None => diff.added.push(asset.clone()),
            Some(entry) => {
                if entry_version_changed(entry, asset) {
                    diff.modified.push(asset.clone());
                }
            }
        }
        current_keys.insert(key);
    }

    for entry in &checkpoint.entries {
        if !current_keys.contains(&entry.key) {
            diff.removed.push(entry.clone());
        }
    }

    diff
}

/// Filter a per-tab asset list to only the caches that appeared or changed
/// after the active checkpoint. Returns the list unchanged when no checkpoint
/// is active (or the active checkpoint no longer exists).
pub fn filter_for_view(assets: Arc<Vec<AssetInfo>>) -> Arc<Vec<AssetInfo>> {
    let Some(active_id) = ACTIVE_CHECKPOINT.lock().unwrap().clone() else {
        return assets;
    };
    let checkpoints = CHECKPOINTS.lock().unwrap();
    let Some(checkpoint) = checkpoints.iter().find(|cp| cp.id == active_id) else {
        return assets;
    };

    let diff = diff_checkpoint(checkpoint, &assets);
    let mut result = diff.added;
    result.extend(diff.modified);
    Arc::new(result)
}

pub fn get_checkpoints() -> Vec<CacheCheckpoint> {
    CHECKPOINTS.lock().unwrap().clone()
}

pub fn get_active_checkpoint_id() -> Option<String> {
    ACTIVE_CHECKPOINT.lock().unwrap().clone()
}

pub fn set_active_checkpoint(id: Option<String>) {
    *ACTIVE_CHECKPOINT.lock().unwrap() = id.clone();
    match id {
        Some(id) => config::set_config_value(ACTIVE_CHECKPOINT_KEY, id.into()),
        None => config::remove_config_value(ACTIVE_CHECKPOINT_KEY),
    }
    config::save_config_file();
}

pub fn rename_checkpoint(id: &str, name: &str) -> bool {
    let mut checkpoints = CHECKPOINTS.lock().unwrap();
    let Some(checkpoint) = checkpoints.iter_mut().find(|cp| cp.id == id) else {
        return false;
    };
    checkpoint.name = name.to_owned();
    drop(checkpoints);
    persist_checkpoints();
    true
}

pub fn delete_checkpoint(id: &str) -> bool {
    let mut checkpoints = CHECKPOINTS.lock().unwrap();
    let len_before = checkpoints.len();
    checkpoints.retain(|cp| cp.id != id);
    let changed = checkpoints.len() != len_before;
    drop(checkpoints);

    if changed {
        persist_checkpoints();
        let mut active = ACTIVE_CHECKPOINT.lock().unwrap();
        if active.as_deref() == Some(id) {
            *active = None;
            config::remove_config_value(ACTIVE_CHECKPOINT_KEY);
            config::save_config_file();
        }
    }
    changed
}

/// Refresh the Music and All lists and return the union of both, so a
/// checkpoint snapshot always covers the whole cache (sounds + http + SQL +
/// rbx-storage), not only the currently visible tab.
fn refresh_full_cache() -> Vec<AssetInfo> {
    logic::refresh(Category::Music, false, true);
    let music = logic::get_file_list().to_vec();

    logic::refresh(Category::All, false, true);
    let all = logic::get_file_list().to_vec();

    let mut result = all;
    let mut keys: HashSet<String> = result.iter().map(asset_key).collect();
    for asset in music {
        let key = asset_key(&asset);
        if keys.insert(key) {
            result.push(asset);
        }
    }
    result
}

pub fn set_current_cache(assets: Vec<AssetInfo>) {
    *CURRENT_CACHE.lock().unwrap() = Arc::new(assets);
    CURRENT_GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn get_current_cache() -> Arc<Vec<AssetInfo>> {
    CURRENT_CACHE.lock().unwrap().clone()
}

pub fn get_current_generation() -> u64 {
    CURRENT_GENERATION.load(Ordering::Relaxed)
}

/// Asynchronously refresh the full cache state used by the checkpoint UI.
/// No-op while another task (extract/checkpoint) is running.
pub fn refresh_current_cache_async() {
    if logic::get_task_running() {
        return;
    }
    logic::set_task_running(true);
    std::thread::spawn(move || {
        let current = refresh_full_cache();
        set_current_cache(current);
        logic::set_task_running(false);
    });
}

/// Capture the full current cache state as a new checkpoint and make it the
/// active one. Returns the new checkpoint id, or `None` when another task is
/// running. The snapshot itself runs on a background thread; the returned id
/// is valid once `checkpoint-created` is shown in the status bar (or the
/// checkpoint appears in the list).
pub fn create_checkpoint() -> Option<String> {
    if logic::get_task_running() {
        return None;
    }
    let id = new_checkpoint_id();
    let locale = locale::get_locale(None);
    let index = CHECKPOINTS.lock().unwrap().len() + 1;
    let name = format!(
        "{} {index}",
        locale::get_message(&locale, "checkpoint-default-name", None)
    );

    logic::set_task_running(true);
    logic::update_status(locale::get_message(&locale, "checkpoint-creating", None));

    std::thread::spawn(move || {
        let current = refresh_full_cache();
        let checkpoint = CacheCheckpoint {
            id: id.clone(),
            name,
            created_at_secs: now_secs(),
            entries: current.iter().map(checkpoint_entry).collect(),
        };
        CHECKPOINTS.lock().unwrap().push(checkpoint);
        persist_checkpoints();
        set_active_checkpoint(Some(id.clone()));
        set_current_cache(current);
        logic::set_task_running(false);
        logic::update_status(locale::get_message(&locale, "checkpoint-created", None));
    });

    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn asset(name: &str, category: Category, size: u64, fingerprint: Option<u64>) -> AssetInfo {
        AssetInfo {
            name: name.to_owned(),
            _size: size,
            last_modified: Some(UNIX_EPOCH + Duration::from_secs(100)),
            from_file: true,
            from_sql: false,
            from_rbx_storage: false,
            category,
            fingerprint,
            file_type: Some("PNG".to_owned()),
            extension: Some("png".to_owned()),
            detected_at: Some(UNIX_EPOCH),
        }
    }

    fn snapshot(assets: &[AssetInfo]) -> Vec<CheckpointEntry> {
        assets.iter().map(checkpoint_entry).collect()
    }

    fn checkpoint(entries: Vec<CheckpointEntry>) -> CacheCheckpoint {
        CacheCheckpoint {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            created_at_secs: 0,
            entries,
        }
    }

    #[test]
    fn test_asset_key_is_stable_and_unique() {
        let a = asset("abc", Category::Images, 1, Some(1));
        let mut b = asset("abc", Category::Images, 1, Some(1));
        b.from_file = false;
        b.from_sql = true;
        assert_ne!(asset_key(&a), asset_key(&b), "different sources must differ");
        assert_eq!(asset_key(&a), asset_key(&a));
        // Name changes must change the key, but index/order must not.
        let b2 = asset("def", Category::Images, 1, Some(1));
        assert_ne!(asset_key(&a), asset_key(&b2));
    }

    #[test]
    fn test_diff_added_modified_removed() {
        // Snapshot: A (unchanged), B (will change), C (will be removed/vanish).
        let cache_a = asset("A", Category::Images, 10, Some(111));
        let cache_b = asset("B", Category::Sounds, 20, Some(222));
        let cache_c = asset("C", Category::Ktx, 30, Some(333));
        let entries = snapshot(&[cache_a, cache_b, cache_c]);

        // Current: A unchanged, B changed content, C gone, D newly added.
        let mut current_b = asset("B", Category::Sounds, 20, Some(222));
        current_b._size = 25; // changed
        let current = vec![
            cache_a.clone(),
            current_b,
            asset("D", Category::Images, 40, Some(444)),
        ];

        let diff = diff_checkpoint(&checkpoint(entries), &current);

        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].name, "D");
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.modified[0].name, "B");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].name, "C");

        // An identical current state produces an empty diff.
        let diff = diff_checkpoint(&checkpoint(snapshot(&[cache_a, cache_b, cache_c])), &current);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.removed.len(), 1);
    }

    #[test]
    fn test_entry_version_changed() {
        let entry = checkpoint_entry(&asset("A", Category::Images, 10, Some(111)));
        assert!(!entry_version_changed(&entry, &asset("A", Category::Images, 10, Some(111))));
        assert!(entry_version_changed(&entry, &asset("A", Category::Images, 11, Some(111))), "size");
        assert!(entry_version_changed(&entry, &asset("A", Category::Images, 10, Some(222))), "fingerprint");

        // None fingerprint + same metadata = unchanged (music-style records).
        let none_entry = checkpoint_entry(&asset("M", Category::Music, 5, None));
        assert!(!entry_version_changed(&none_entry, &asset("M", Category::Music, 5, None)));
        assert!(entry_version_changed(&none_entry, &asset("M", Category::Music, 6, None)));
    }
}
