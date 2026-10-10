//! Which tabs each agent opened (M2a §3).
//!
//! In memory only, and deliberately so. A tab id is a number the browser
//! hands out, and Firefox counts from small numbers again after a restart: a
//! table that outlived the browser would point an agent at whichever of the
//! person's tabs now carries its old number. So the table belongs to the
//! browser connection that served it — when that connection goes away, or
//! calls start being answered by a different one, the agent's tabs are
//! forgotten and its next navigate opens a fresh one.
//!
//! The primary tab is the one opened most recently; a call that names no tab
//! runs there.
//!
//! The same table remembers which ids the agent may name at all: its own
//! tabs and every tab a `browser_get_tabs` result showed it in this browser
//! session. An id from anywhere else — most often from before a restart —
//! is refused rather than sent (spec goal 4).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// One tab an agent opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedTab {
    pub id: i64,
    /// Where it was last sent, for the approval question. `None` when the
    /// browser did not say.
    pub url: Option<String>,
}

struct Owned {
    /// The browser connection these ids belong to.
    served_by: String,
    /// Oldest first; the last is primary.
    tabs: Vec<OwnedTab>,
    /// Ids a `browser_get_tabs` result showed this agent here.
    listed: HashSet<i64>,
}

/// Every agent's tabs, keyed by the pairing's control channel id.
#[derive(Default)]
pub struct AgentTabs {
    by_pairing: Mutex<HashMap<String, Owned>>,
}

impl AgentTabs {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Owned>> {
        self.by_pairing.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The pairing's table as `served_by` sees it. A table that belongs to
    /// another connection is dropped on sight, not merely hidden.
    fn current<'a>(
        map: &'a mut HashMap<String, Owned>,
        pairing: &str,
        served_by: &str,
    ) -> Option<&'a mut Owned> {
        if map.get(pairing).is_some_and(|o| o.served_by != served_by) {
            map.remove(pairing);
        }
        map.get_mut(pairing)
    }

    /// The agent's primary tab in the browser `served_by` names.
    pub fn primary(&self, pairing: &str, served_by: &str) -> Option<OwnedTab> {
        let mut map = self.lock();
        Self::current(&mut map, pairing, served_by).and_then(|o| o.tabs.last().cloned())
    }

    /// Every tab the agent owns there, oldest first.
    pub fn owned_ids(&self, pairing: &str, served_by: &str) -> Vec<i64> {
        let mut map = self.lock();
        Self::current(&mut map, pairing, served_by)
            .map(|o| o.tabs.iter().map(|t| t.id).collect())
            .unwrap_or_default()
    }

    /// Where an owned tab was last sent.
    pub fn url_of(&self, pairing: &str, tab_id: i64) -> Option<String> {
        self.lock()
            .get(pairing)?
            .tabs
            .iter()
            .find(|t| t.id == tab_id)?
            .url
            .clone()
    }

    /// The pairing's table under `served_by`, started empty if there is
    /// none (or only another browser's).
    fn current_or_new<'a>(
        map: &'a mut HashMap<String, Owned>,
        pairing: &str,
        served_by: &str,
    ) -> &'a mut Owned {
        if Self::current(map, pairing, served_by).is_none() {
            map.insert(
                pairing.to_string(),
                Owned {
                    served_by: served_by.to_string(),
                    tabs: Vec::new(),
                    listed: HashSet::new(),
                },
            );
        }
        map.get_mut(pairing).expect("just inserted")
    }

    /// The agent opened `tab_id`; it becomes primary.
    pub fn record_opened(&self, pairing: &str, served_by: &str, tab_id: i64, url: Option<String>) {
        let mut map = self.lock();
        let owned = Self::current_or_new(&mut map, pairing, served_by);
        owned.tabs.retain(|t| t.id != tab_id);
        owned.tabs.push(OwnedTab { id: tab_id, url });
    }

    /// A `browser_get_tabs` result from `served_by` showed the agent these
    /// tabs; it may name them from now on, in that browser only.
    pub fn record_listed(&self, pairing: &str, served_by: &str, ids: &[i64]) {
        let mut map = self.lock();
        Self::current_or_new(&mut map, pairing, served_by)
            .listed
            .extend(ids.iter().copied());
    }

    /// Whether the agent may name `tab_id` in the browser `served_by` names:
    /// it opened the tab there, or was shown it there by `browser_get_tabs`.
    pub fn is_known(&self, pairing: &str, served_by: &str, tab_id: i64) -> bool {
        let mut map = self.lock();
        Self::current(&mut map, pairing, served_by)
            .is_some_and(|o| o.listed.contains(&tab_id) || o.tabs.iter().any(|t| t.id == tab_id))
    }

    /// An owned tab was navigated. A tab the agent does not own is left
    /// alone: navigating someone else's tab does not make it the agent's.
    pub fn record_url(&self, pairing: &str, tab_id: i64, url: Option<String>) {
        if let Some(tab) = self
            .lock()
            .get_mut(pairing)
            .and_then(|o| o.tabs.iter_mut().find(|t| t.id == tab_id))
        {
            tab.url = url;
        }
    }

    /// The tab is gone. Reports whether the agent owned it.
    pub fn forget_tab(&self, pairing: &str, tab_id: i64) -> bool {
        let mut map = self.lock();
        let Some(owned) = map.get_mut(pairing) else {
            return false;
        };
        owned.listed.remove(&tab_id);
        let before = owned.tabs.len();
        owned.tabs.retain(|t| t.id != tab_id);
        owned.tabs.len() != before
    }

    /// Forget everything one pairing owned.
    pub fn forget_pairing(&self, pairing: &str) {
        self.lock().remove(pairing);
    }

    /// A browser connection went away: every table it served is forgotten
    /// (M2a §3.3). Reports how many pairings lost their tabs.
    pub fn connection_closed(&self, proxy_id: &str) -> usize {
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, o| o.served_by != proxy_id);
        before - map.len()
    }
}

/// The daemon's one table, shared by every agent pairing and by the
/// connection cleanup in `server.rs`.
pub fn shared() -> Arc<AgentTabs> {
    static SHARED: OnceLock<Arc<AgentTabs>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(AgentTabs::new())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "pair-a";
    const B: &str = "pair-b";
    const S1: &str = "proxy-1";
    const S2: &str = "proxy-2";

    fn url(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn nothing_is_owned_until_an_agent_opens_a_tab() {
        let t = AgentTabs::new();
        assert_eq!(t.primary(A, S1), None);
        assert!(t.owned_ids(A, S1).is_empty());
    }

    #[test]
    fn the_most_recent_tab_is_primary() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, url("https://a.example/"));
        t.record_opened(A, S1, 11, url("https://b.example/"));
        assert_eq!(
            t.primary(A, S1),
            Some(OwnedTab {
                id: 11,
                url: url("https://b.example/")
            })
        );
        assert_eq!(t.owned_ids(A, S1), vec![10, 11]);
    }

    #[test]
    fn reopening_a_tab_moves_it_to_primary_without_duplicating_it() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        t.record_opened(A, S1, 11, None);
        t.record_opened(A, S1, 10, None);
        assert_eq!(t.primary(A, S1).unwrap().id, 10);
        assert_eq!(t.owned_ids(A, S1), vec![11, 10]);
    }

    #[test]
    fn a_closed_primary_hands_over_to_the_one_before() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        t.record_opened(A, S1, 11, None);
        assert!(t.forget_tab(A, 11));
        assert_eq!(t.primary(A, S1).unwrap().id, 10);
        assert!(!t.forget_tab(A, 11), "already gone");
    }

    #[test]
    fn a_navigation_updates_the_remembered_url() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, url("https://a.example/"));
        t.record_url(A, 10, url("https://b.example/"));
        assert_eq!(t.url_of(A, 10), url("https://b.example/"));
        t.record_url(A, 99, url("https://c.example/"));
        assert_eq!(
            t.url_of(A, 99),
            None,
            "a tab the agent never opened is not adopted"
        );
    }

    #[test]
    fn a_browser_that_disconnects_takes_its_tab_numbers_with_it() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        t.record_opened(B, S2, 20, None);
        assert_eq!(t.connection_closed(S1), 1);
        assert_eq!(t.primary(A, S1), None);
        assert_eq!(
            t.primary(B, S2).unwrap().id,
            20,
            "another browser's tabs stay"
        );
        assert_eq!(t.connection_closed(S1), 0);
    }

    #[test]
    fn calls_answered_by_another_browser_forget_the_old_tabs() {
        // Firefox numbers tabs from small integers again after a restart: an
        // id remembered from one connection means nothing on the next.
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        assert_eq!(t.primary(A, S2), None);
        assert_eq!(t.primary(A, S1), None, "dropped, not merely hidden");
    }

    #[test]
    fn opening_a_tab_in_a_new_browser_starts_a_new_table() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        t.record_opened(A, S2, 5, None);
        assert_eq!(t.owned_ids(A, S2), vec![5]);
    }

    #[test]
    fn pairings_never_see_each_others_tabs() {
        let t = AgentTabs::new();
        t.record_opened(A, S1, 10, None);
        t.record_opened(B, S1, 20, None);
        assert_eq!(t.owned_ids(A, S1), vec![10]);
        assert_eq!(t.owned_ids(B, S1), vec![20]);
        t.forget_pairing(A);
        assert!(t.owned_ids(A, S1).is_empty());
        assert_eq!(t.owned_ids(B, S1), vec![20]);
    }

    #[test]
    fn only_owned_and_listed_ids_are_known() {
        let t = AgentTabs::new();
        assert!(!t.is_known(A, S1, 1));
        t.record_opened(A, S1, 10, None);
        t.record_listed(A, S1, &[1, 2]);
        assert!(t.is_known(A, S1, 10));
        assert!(t.is_known(A, S1, 1));
        assert!(!t.is_known(A, S1, 3));
        assert!(!t.is_known(B, S1, 1), "pairings never share a listing");
        assert_eq!(t.primary(A, S1).unwrap().id, 10, "listing adopts nothing");
        assert!(t.forget_tab(A, 10));
        assert!(!t.forget_tab(A, 1), "a listed tab was never owned");
        assert!(!t.is_known(A, S1, 10) && !t.is_known(A, S1, 1));
        assert!(t.is_known(A, S1, 2));
    }

    #[test]
    fn listed_ids_go_with_the_browser_that_listed_them() {
        let t = AgentTabs::new();
        t.record_listed(A, S1, &[1]);
        assert_eq!(t.connection_closed(S1), 1);
        assert!(!t.is_known(A, S1, 1));

        t.record_listed(A, S1, &[1]);
        assert!(!t.is_known(A, S2, 1));
        assert!(!t.is_known(A, S1, 1), "dropped, not merely hidden");

        t.record_listed(A, S1, &[1]);
        t.forget_pairing(A);
        assert!(!t.is_known(A, S1, 1));
    }

    #[test]
    fn the_shared_table_is_one_table() {
        assert!(Arc::ptr_eq(&shared(), &shared()));
    }
}
