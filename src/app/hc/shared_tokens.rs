//! A pane's metadata tokens (a plugin's marks on the pane it opened, such as
//! "this is my sidebar", with a heartbeat time) are reported to one viewer,
//! but every viewer of the same layout acts on them: a sidebar's `--ensure`
//! run by another viewer must see that the tab already has its sidebar, or
//! it opens a second one. So the tokens are also kept on the hc terminal,
//! as one label every viewer reads (`TerminalState::shared_tokens`).
use super::*;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// A heartbeat that only moves a time on is written at most this often: well
/// inside a plugin's staleness bound (herdr-sidebar: 20 s), without a label
/// write per heartbeat per pane.
const REFRESH: Duration = Duration::from_secs(8);

#[derive(Default)]
pub(crate) struct SharedTokens {
    /// What was last written for each hc address, and when.
    written: HashMap<String, (HashMap<String, String>, Instant)>,
}

/// Whether `now` must be written over `last`: at once when tokens appear,
/// disappear or change otherwise; only a time moving on waits for `REFRESH`.
fn due(
    last: Option<&(HashMap<String, String>, Instant)>,
    now: &HashMap<String, String>,
    at: Instant,
) -> bool {
    let Some((before, written_at)) = last else {
        return true;
    };
    if before == now {
        return false;
    }
    let only_times = before.len() == now.len()
        && now.iter().all(|(key, value)| {
            before.get(key).is_some_and(|old| {
                old == value || (old.parse::<u64>().is_ok() && value.parse::<u64>().is_ok())
            })
        });
    !only_times || at.duration_since(*written_at) >= REFRESH
}

impl App {
    /// Keeps a pane's tokens on its hc terminal for the other viewers.
    pub(crate) fn share_hc_tokens(&mut self, terminal_id: &crate::terminal::TerminalId) {
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return;
        };
        let Some(snapshot) = terminal.hc_snapshot.as_ref() else {
            return;
        };
        let address = snapshot.binding.address.clone();
        let tokens = terminal.metadata_tokens.values();
        let now = Instant::now();
        if !due(self.shared_tokens.written.get(&address), &tokens, now) {
            return;
        }
        let value = if tokens.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&tokens).unwrap_or_default()
        };
        self.shared_tokens
            .written
            .insert(address.clone(), (tokens, now));
        let mut labels = serde_json::Map::new();
        labels.insert(
            crate::terminal::SHARED_TOKENS_LABEL.into(),
            serde_json::Value::from(value),
        );
        self.terminal_factory.label_hc_terminal(&address, labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn heartbeats_are_shared_often_enough_but_not_each_one() {
        let start = Instant::now();
        let first = tokens(&[("herdr-sidebar-explorer", "100")]);
        assert!(due(None, &first, start));
        let last = (first, start);
        // Same tokens: nothing to write.
        assert!(!due(
            Some(&last),
            &tokens(&[("herdr-sidebar-explorer", "100")]),
            start
        ));
        // Only the time moved on: waits for the refresh interval.
        let beat = tokens(&[("herdr-sidebar-explorer", "105")]);
        assert!(!due(Some(&last), &beat, start + Duration::from_secs(5)));
        assert!(due(Some(&last), &beat, start + REFRESH));
        // A token appears or goes: at once.
        let more = tokens(&[
            ("herdr-sidebar-explorer", "105"),
            ("herdr-sidebar-git", "105"),
        ]);
        assert!(due(Some(&last), &more, start + Duration::from_secs(1)));
        assert!(due(
            Some(&last),
            &HashMap::new(),
            start + Duration::from_secs(1)
        ));
        // A value that is not a time changes: at once.
        let flag = (tokens(&[("mode", "a")]), start);
        assert!(due(Some(&flag), &tokens(&[("mode", "b")]), start));
    }
}
