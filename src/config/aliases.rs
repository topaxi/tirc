use std::collections::HashMap;
use std::path::PathBuf;

/// Persisted `:alias` buffer names, keyed by backend *name* (IRC host, Matrix
/// homeserver, Mattermost url) rather than [`crate::core::BackendId`], which is
/// a runtime index into `config.servers` and would shift when servers are
/// reordered. Entries for servers not currently configured are retained across
/// saves so disabling a server never loses its aliases. Two configured servers
/// with an identical name share aliases.
#[derive(Debug, Default)]
pub struct AliasStore {
    /// `None` when the XDG state dir could not be resolved; saves become no-ops.
    path: Option<PathBuf>,
    /// server name -> target -> alias.
    servers: HashMap<String, HashMap<String, String>>,
}

impl AliasStore {
    /// Loads `aliases.json` from the XDG state dir. A missing or unreadable
    /// file yields an empty store (first run / never used).
    pub fn load() -> AliasStore {
        match xdg::BaseDirectories::with_prefix("tirc").place_state_file("aliases.json") {
            Ok(path) => Self::load_from(path),
            Err(err) => {
                log::warn!("unable to resolve state dir for aliases.json: {err}");
                AliasStore::default()
            }
        }
    }

    fn load_from(path: PathBuf) -> AliasStore {
        let servers = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|err| {
                log::warn!("ignoring malformed {}: {err}", path.display());
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        AliasStore {
            path: Some(path),
            servers,
        }
    }

    /// The persisted aliases for one server, as `(target, alias)` pairs.
    pub fn aliases_for(&self, server: &str) -> impl Iterator<Item = (&str, &str)> {
        self.servers
            .get(server)
            .into_iter()
            .flat_map(|targets| targets.iter().map(|(t, a)| (t.as_str(), a.as_str())))
    }

    /// Inserts an alias and immediately persists. Errors are the caller's to
    /// report; the in-memory entry is kept either way.
    pub fn set(&mut self, server: &str, target: &str, name: &str) -> anyhow::Result<()> {
        self.servers
            .entry(server.to_string())
            .or_default()
            .insert(target.to_string(), name.to_string());
        self.save()
    }

    /// Removes an alias and immediately persists. No-op when absent.
    pub fn remove(&mut self, server: &str, target: &str) -> anyhow::Result<()> {
        let Some(targets) = self.servers.get_mut(server) else {
            return Ok(());
        };
        if targets.remove(target).is_none() {
            return Ok(());
        }
        if targets.is_empty() {
            self.servers.remove(server);
        }
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(&self.servers)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_from_json(json: &str) -> AliasStore {
        AliasStore {
            path: None,
            servers: serde_json::from_str(json).unwrap(),
        }
    }

    #[test]
    fn aliases_for_returns_only_the_requested_server() {
        let store = store_from_json(
            r##"{
                "irc.libera.chat": { "#rust-beginners": "rust-101" },
                "https://matrix.org": { "!abc:matrix.org": "friends" }
            }"##,
        );

        let mut libera: Vec<_> = store.aliases_for("irc.libera.chat").collect();
        libera.sort();
        assert_eq!(libera, [("#rust-beginners", "rust-101")]);
        assert_eq!(store.aliases_for("unknown").count(), 0);
    }

    #[test]
    fn set_and_remove_roundtrip() {
        let mut store = AliasStore::default();
        store.set("irc.libera.chat", "#a", "alpha").unwrap();
        store.set("irc.libera.chat", "#a", "renamed").unwrap();

        assert_eq!(
            store.aliases_for("irc.libera.chat").collect::<Vec<_>>(),
            [("#a", "renamed")]
        );

        store.remove("irc.libera.chat", "#a").unwrap();
        assert_eq!(store.aliases_for("irc.libera.chat").count(), 0);
        // The emptied server entry is pruned so the file stays minimal.
        assert!(store.servers.is_empty());
    }

    #[test]
    fn remove_keeps_unrelated_servers() {
        let mut store = store_from_json(
            r##"{
                "irc.libera.chat": { "#a": "alpha" },
                "irc.oftc.net": { "#b": "beta" }
            }"##,
        );

        store.remove("irc.libera.chat", "#a").unwrap();
        store.remove("irc.libera.chat", "#missing").unwrap();

        assert_eq!(
            store.aliases_for("irc.oftc.net").collect::<Vec<_>>(),
            [("#b", "beta")]
        );
    }
}
