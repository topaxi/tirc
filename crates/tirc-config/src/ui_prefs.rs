use std::path::PathBuf;

/// Persisted runtime UI preferences (currently the `:barstyle` buffer-bar
/// layout), stored as a JSON object in the XDG state dir so future prefs can
/// join without a new file. Runtime prefs win over theme options.
#[derive(Debug, Default)]
pub struct UiPrefsStore {
    /// `None` when the XDG state dir could not be resolved; saves become no-ops.
    path: Option<PathBuf>,
    prefs: UiPrefs,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UiPrefs {
    #[serde(skip_serializing_if = "Option::is_none")]
    buffer_bar: Option<String>,
}

impl UiPrefsStore {
    /// Loads `ui_prefs.json` from the XDG state dir. A missing or unreadable
    /// file yields an empty store (first run / never used).
    pub fn load() -> UiPrefsStore {
        match xdg::BaseDirectories::with_prefix("tirc").place_state_file("ui_prefs.json") {
            Ok(path) => Self::load_from(path),
            Err(err) => {
                log::warn!("unable to resolve state dir for ui_prefs.json: {err}");
                UiPrefsStore::default()
            }
        }
    }

    fn load_from(path: PathBuf) -> UiPrefsStore {
        let prefs = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|err| {
                log::warn!("ignoring malformed {}: {err}", path.display());
                UiPrefs::default()
            }),
            Err(_) => UiPrefs::default(),
        };
        UiPrefsStore {
            path: Some(path),
            prefs,
        }
    }

    /// The persisted `:barstyle` override, if any.
    pub fn buffer_bar(&self) -> Option<&str> {
        self.prefs.buffer_bar.as_deref()
    }

    /// Sets (or clears, with `None`) the buffer-bar style and immediately
    /// persists. Errors are the caller's to report; the in-memory value is
    /// kept either way.
    pub fn set_buffer_bar(&mut self, style: Option<&str>) -> anyhow::Result<()> {
        self.prefs.buffer_bar = style.map(str::to_string);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(&self.prefs)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_clear_buffer_bar() {
        let mut store = UiPrefsStore::default();
        assert_eq!(store.buffer_bar(), None);

        store.set_buffer_bar(Some("tabbed")).unwrap();
        assert_eq!(store.buffer_bar(), Some("tabbed"));

        store.set_buffer_bar(None).unwrap();
        assert_eq!(store.buffer_bar(), None);
    }

    #[test]
    fn prefs_roundtrip_through_json() {
        let prefs = UiPrefs {
            buffer_bar: Some("per-backend".to_string()),
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: UiPrefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.buffer_bar.as_deref(), Some("per-backend"));

        // A cleared pref serializes to an empty object and reads back as None.
        let empty: UiPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.buffer_bar, None);
    }

    #[test]
    fn malformed_json_yields_default() {
        let prefs: UiPrefs = serde_json::from_str("not json").unwrap_or_default();
        assert_eq!(prefs.buffer_bar, None);
    }
}
