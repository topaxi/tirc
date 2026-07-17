//! On-disk persistence for parsed link previews. The renderer holds fetched
//! [`LinkPreview`]s only in memory, so without this every restart refetches every
//! visible link over the network. This store serializes the parsed Open Graph
//! metadata (and negative results) as a JSON map in the preview **cache** dir,
//! keyed by URL and carrying a fetch timestamp for date-based eviction.
//!
//! Mirrors the persisted-store idiom in `tirc_config::ui_prefs`: an
//! `Option<PathBuf>` path so a missing dir makes saves no-ops, a lenient load
//! that warns-and-defaults on corruption, and JSON via `serde_json`. It lives in
//! the cache dir (evictable) rather than the state dir, alongside the `og:image`
//! files it references.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::preview::LinkPreview;

/// How long a successful preview stays valid before it is evicted and refetched.
const SUCCESS_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// How long a negative (failed / no-OG) result is remembered, so dead or
/// metadata-less links are not re-hit (each a slow timeout) on every restart,
/// while a transiently-down site recovers the next day.
const FAILED_TTL_SECS: u64 = 24 * 60 * 60;

/// The JSON file, relative to the preview cache dir.
const METADATA_FILE: &str = "metadata.json";

/// Persistent URL -> preview cache backed by a single JSON file in the preview
/// cache dir. Writes are debounced via [`Self::flush`] so a burst of results (a
/// busy buffer opening) does not rewrite the file once per URL.
#[derive(Debug, Default)]
pub struct PreviewCacheStore {
    /// `<cache_dir>/metadata.json`; `None` when the cache dir is unusable, which
    /// makes saves silent no-ops.
    path: Option<PathBuf>,
    /// Cache dir, used to resolve `image_file` <-> absolute `image_path`.
    cache_dir: PathBuf,
    entries: HashMap<String, CacheEntry>,
    /// Set on insert or eviction, cleared on flush.
    dirty: bool,
}

/// One cached URL: when it was fetched and, on success, its parsed preview. A
/// `None` `preview` is a remembered negative result.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    preview: Option<CachedPreview>,
}

/// Serializable mirror of [`LinkPreview`]. The image is stored as a file **name**
/// within the cache dir (not an absolute path) so the cache stays portable if the
/// dir moves.
#[derive(Debug, Serialize, Deserialize)]
struct CachedPreview {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_file: Option<String>,
    /// Pixel dimensions of the thumbnail, so the renderer can reserve its cell
    /// height on the first frame after a restart without waiting for a decode.
    #[serde(skip_serializing_if = "Option::is_none")]
    image_w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_h: Option<u32>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PreviewCacheStore {
    /// Loads `metadata.json` from `cache_dir` and evicts stale entries. A missing
    /// or unreadable file yields an empty store; a malformed file is warned about
    /// and treated as empty.
    pub fn load(cache_dir: PathBuf) -> PreviewCacheStore {
        let path = cache_dir.join(METADATA_FILE);
        let entries = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|err| {
                log::warn!("ignoring malformed {}: {err}", path.display());
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        let mut store = PreviewCacheStore {
            path: Some(path),
            cache_dir,
            entries,
            dirty: false,
        };
        store.evict_stale();
        store
    }

    /// Drops entries past their TTL (7d for successes, 1d for negatives),
    /// best-effort deleting the orphaned image file of each evicted success so the
    /// image dir does not grow forever. Marks the store dirty if anything changed.
    fn evict_stale(&mut self) {
        let now = now_secs();
        let cache_dir = &self.cache_dir;
        let mut removed = false;
        self.entries.retain(|_url, entry| {
            let ttl = if entry.preview.is_some() {
                SUCCESS_TTL_SECS
            } else {
                FAILED_TTL_SECS
            };
            let fresh = now.saturating_sub(entry.fetched_at) <= ttl;
            if !fresh {
                if let Some(file) = entry.preview.as_ref().and_then(|p| p.image_file.as_ref()) {
                    let _ = std::fs::remove_file(cache_dir.join(file));
                }
                removed = true;
            }
            fresh
        });
        if removed {
            self.dirty = true;
        }
    }

    /// Surviving successful previews as `(url, LinkPreview)`, for seeding the
    /// renderer's in-memory cache. An `image_file` that no longer exists on disk
    /// is downgraded to a text-only preview rather than a dangling path.
    pub fn seed_success(&self) -> Vec<(String, LinkPreview)> {
        self.entries
            .iter()
            .filter_map(|(url, entry)| {
                let preview = entry.preview.as_ref()?;
                Some((url.clone(), preview.to_link_preview(&self.cache_dir)))
            })
            .collect()
    }

    /// URLs of surviving negative results, for seeding the renderer's failed set
    /// so they are not refetched.
    pub fn seed_failed(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.preview.is_none())
            .map(|(url, _)| url.clone())
            .collect()
    }

    /// Records a successful preview for `url` with a fresh timestamp.
    pub fn insert_success(&mut self, url: String, preview: &LinkPreview) {
        self.entries.insert(
            url,
            CacheEntry {
                fetched_at: now_secs(),
                preview: Some(CachedPreview::from_link_preview(preview, &self.cache_dir)),
            },
        );
        self.dirty = true;
    }

    /// Records a negative result for `url` with a fresh timestamp.
    pub fn insert_failed(&mut self, url: String) {
        self.entries.insert(
            url,
            CacheEntry {
                fetched_at: now_secs(),
                preview: None,
            },
        );
        self.dirty = true;
    }

    /// Writes the cache to disk if it changed since the last flush. Errors are
    /// logged, not propagated: a cache write failure must never disrupt the UI.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else {
            self.dirty = false;
            return;
        };
        match serde_json::to_string_pretty(&self.entries) {
            Ok(json) => {
                if let Err(err) = std::fs::write(path, json) {
                    log::warn!("failed to write preview cache {}: {err}", path.display());
                }
            }
            Err(err) => log::warn!("failed to serialize preview cache: {err}"),
        }
        self.dirty = false;
    }
}

impl CachedPreview {
    fn from_link_preview(preview: &LinkPreview, cache_dir: &Path) -> CachedPreview {
        let image_file = preview.image_path.as_ref().map(|path| {
            path.strip_prefix(cache_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        });
        CachedPreview {
            title: preview.title.clone(),
            description: preview.description.clone(),
            site_name: preview.site_name.clone(),
            image_file,
            image_w: preview.image_dims.map(|(w, _)| w),
            image_h: preview.image_dims.map(|(_, h)| h),
        }
    }

    fn to_link_preview(&self, cache_dir: &Path) -> LinkPreview {
        let image_path = self.image_file.as_ref().and_then(|file| {
            let path = cache_dir.join(file);
            path.exists().then_some(path)
        });
        // Only report dimensions when the image is still on disk, so the renderer
        // never reserves rows for a thumbnail it cannot draw.
        let image_dims = image_path
            .as_ref()
            .and(self.image_w.zip(self.image_h));
        LinkPreview {
            title: self.title.clone(),
            description: self.description.clone(),
            site_name: self.site_name.clone(),
            image_path,
            image_dims,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fetched_at: u64, preview: Option<CachedPreview>) -> CacheEntry {
        CacheEntry {
            fetched_at,
            preview,
        }
    }

    fn success_preview() -> CachedPreview {
        CachedPreview {
            title: Some("Never Gonna Give You Up".to_string()),
            description: Some("The official video".to_string()),
            site_name: Some("YouTube".to_string()),
            image_file: None,
            image_w: None,
            image_h: None,
        }
    }

    #[test]
    fn entries_roundtrip_through_json() {
        let mut entries = HashMap::new();
        entries.insert(
            "https://example.com/a".to_string(),
            entry(1_700_000_000, Some(success_preview())),
        );
        entries.insert(
            "https://example.com/dead".to_string(),
            entry(1_700_000_000, None),
        );

        let json = serde_json::to_string(&entries).unwrap();
        let back: HashMap<String, CacheEntry> = serde_json::from_str(&json).unwrap();

        let ok = &back["https://example.com/a"];
        assert_eq!(
            ok.preview.as_ref().unwrap().title.as_deref(),
            Some("Never Gonna Give You Up")
        );
        assert!(back["https://example.com/dead"].preview.is_none());
    }

    #[test]
    fn eviction_drops_stale_by_ttl() {
        let now = now_secs();
        let mut store = PreviewCacheStore {
            path: None,
            cache_dir: PathBuf::from("/nonexistent"),
            entries: HashMap::new(),
            dirty: false,
        };
        store.entries.insert(
            "fresh-success".to_string(),
            entry(now - 2 * 24 * 60 * 60, Some(success_preview())),
        );
        store.entries.insert(
            "stale-success".to_string(),
            entry(now - 8 * 24 * 60 * 60, Some(success_preview())),
        );
        store.entries.insert(
            "fresh-failure".to_string(),
            entry(now - 60 * 60, None),
        );
        store.entries.insert(
            "stale-failure".to_string(),
            entry(now - 2 * 24 * 60 * 60, None),
        );

        store.evict_stale();

        assert!(store.entries.contains_key("fresh-success"));
        assert!(!store.entries.contains_key("stale-success"));
        assert!(store.entries.contains_key("fresh-failure"));
        assert!(!store.entries.contains_key("stale-failure"));
        assert!(store.dirty, "eviction should mark the store dirty");
    }

    #[test]
    fn malformed_json_yields_empty() {
        let entries: HashMap<String, CacheEntry> =
            serde_json::from_str("not json").unwrap_or_default();
        assert!(entries.is_empty());
    }

    #[test]
    fn image_path_survives_a_roundtrip_via_cache_dir() {
        let cache_dir = PathBuf::from("/cache/tirc/previews");
        let preview = LinkPreview {
            title: Some("t".to_string()),
            description: None,
            site_name: None,
            image_path: Some(cache_dir.join("deadbeef")),
            image_dims: Some((640, 480)),
        };
        let cached = CachedPreview::from_link_preview(&preview, &cache_dir);
        assert_eq!(cached.image_file.as_deref(), Some("deadbeef"));
        assert_eq!(cached.image_w, Some(640));
        assert_eq!(cached.image_h, Some(480));

        // The file does not exist on disk, so it downgrades to text-only and
        // drops the dimensions (nothing to reserve rows for).
        let back = cached.to_link_preview(&cache_dir);
        assert!(back.image_path.is_none());
        assert!(back.image_dims.is_none());
        assert_eq!(back.title.as_deref(), Some("t"));
    }
}
