use std::collections::HashSet;
use std::path::PathBuf;

/// Persisted `:bufmove` buffer order: a flat list of `(server name, target)`
/// pairs in display order, so the exact tab order - including cross-server
/// interleaving - restores across restarts. Keyed by backend *name* for the
/// same reason as [`super::aliases::AliasStore`]. Entries for servers not part
/// of a snapshot (e.g. currently disabled) are retained at the end so their
/// relative order survives.
#[derive(Debug, Default)]
pub struct BufferOrderStore {
    /// `None` when the XDG state dir could not be resolved; saves become no-ops.
    path: Option<PathBuf>,
    /// `(server name, target)` in display order.
    entries: Vec<(String, String)>,
}

impl BufferOrderStore {
    /// Loads `buffer_order.json` from the XDG state dir. A missing or
    /// unreadable file yields an empty store (first run / never used).
    pub fn load() -> BufferOrderStore {
        match xdg::BaseDirectories::with_prefix("tirc").place_state_file("buffer_order.json") {
            Ok(path) => Self::load_from(path),
            Err(err) => {
                log::warn!("unable to resolve state dir for buffer_order.json: {err}");
                BufferOrderStore::default()
            }
        }
    }

    fn load_from(path: PathBuf) -> BufferOrderStore {
        let entries = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|err| {
                log::warn!("ignoring malformed {}: {err}", path.display());
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        BufferOrderStore {
            path: Some(path),
            entries,
        }
    }

    /// The persisted order as `(server name, target)` pairs; the position in
    /// the iteration is the buffer's rank.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(server, target)| (server.as_str(), target.as_str()))
    }

    /// Replaces the order with a snapshot of the current tab order and
    /// immediately persists. Old entries for servers not present in the
    /// snapshot are kept at the end so a disabled server's order survives.
    pub fn set_order(&mut self, snapshot: Vec<(String, String)>) -> anyhow::Result<()> {
        let servers: HashSet<&String> = snapshot.iter().map(|(server, _)| server).collect();
        let retained: Vec<(String, String)> = self
            .entries
            .drain(..)
            .filter(|(server, _)| !servers.contains(server))
            .collect();
        self.entries = snapshot;
        self.entries.extend(retained);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(s, t)| (s.to_string(), t.to_string()))
            .collect()
    }

    #[test]
    fn set_order_replaces_snapshotted_servers_and_retains_others() {
        let mut store = BufferOrderStore {
            path: None,
            entries: pairs(&[
                ("irc.libera.chat", "#old"),
                ("irc.oftc.net", "#kept"),
                ("irc.libera.chat", "#older"),
            ]),
        };

        store
            .set_order(pairs(&[
                ("irc.libera.chat", "#b"),
                ("irc.libera.chat", "#a"),
            ]))
            .unwrap();

        assert_eq!(
            store.iter().collect::<Vec<_>>(),
            [
                ("irc.libera.chat", "#b"),
                ("irc.libera.chat", "#a"),
                ("irc.oftc.net", "#kept"),
            ]
        );
    }

    #[test]
    fn roundtrips_through_json() {
        let entries = pairs(&[("https://matrix.org", "!a:matrix.org"), ("irc", "#x")]);
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<(String, String)> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);
    }
}
