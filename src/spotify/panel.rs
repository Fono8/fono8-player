//! State of the Spotify panel: results, selection, paging and the import target
//! (the counterpart of the YouTube Music panel state).

use std::collections::HashSet;

use crate::i18n::Message;
use crate::library::Track;

use super::api::{Page, Playlist};

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Track(Track),
    Playlist(Playlist),
    /// The user's liked songs, offered next to their playlists.
    Liked,
}

/// What "more results" continues.
#[derive(Clone, Debug, PartialEq)]
pub enum More {
    Search { query: String, offset: u32 },
    Playlists { offset: u32 },
}

pub struct Panel {
    pub busy: bool,
    pub message: Message,
    pub results: Vec<Item>,
    pub selected: HashSet<usize>,
    /// Target playlist for "Add selected"; `None` means the whole library.
    pub target: Option<i64>,
    pub more: Option<More>,
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            busy: false,
            message: Message::new("spotify_intro"),
            results: Vec::new(),
            selected: HashSet::new(),
            target: None,
            more: None,
        }
    }
}

impl Panel {
    pub fn clear(&mut self) {
        self.results.clear();
        self.selected.clear();
        self.more = None;
        self.busy = false;
    }

    /// Search results; later pages are appended.
    pub fn search_results(&mut self, query: String, offset: u32, page: Page<Track>) {
        if offset == 0 {
            self.results.clear();
            self.selected.clear();
        }
        self.results.extend(page.items.into_iter().map(Item::Track));
        self.more = page.next.map(|offset| More::Search { query, offset });
        self.busy = false;
        self.message = Message::new("spotify_results").with("n", self.results.len()).with("total", page.total as usize);
    }

    /// The user's playlists, after "Liked songs"; later pages are appended.
    pub fn playlists(&mut self, page: Page<Playlist>, first: bool) {
        if first {
            self.results = vec![Item::Liked];
            self.selected.clear();
        }
        self.results.extend(page.items.into_iter().map(Item::Playlist));
        self.more = page.next.map(|offset| More::Playlists { offset });
        self.busy = false;
        self.message = Message::new("spotify_playlists_listed");
    }

    pub fn selected_items(&self) -> Vec<Item> {
        let mut indices: Vec<usize> = self.selected.iter().copied().collect();
        indices.sort_unstable();
        indices.into_iter().filter_map(|i| self.results.get(i).cloned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(n: u32) -> Track {
        Track { path: format!("spotify:track:{n:0>22}"), title: format!("T{n}"), ..Default::default() }
    }

    #[test]
    fn search_pages_append_and_selection_follows_result_order() {
        let mut panel = Panel::default();
        panel.search_results("q".into(), 0, Page { items: vec![track(1), track(2)], total: 3, next: Some(2) });
        assert_eq!(panel.more, Some(More::Search { query: "q".into(), offset: 2 }));
        panel.search_results("q".into(), 2, Page { items: vec![track(3)], total: 3, next: None });
        assert_eq!((panel.results.len(), panel.more.clone()), (3, None));
        panel.selected = HashSet::from([0, 2]);
        assert_eq!(panel.selected_items(), [Item::Track(track(1)), Item::Track(track(3))]);
        panel.playlists(Page { items: vec![], total: 0, next: None }, true);
        assert_eq!(panel.results, [Item::Liked]);
        assert!(panel.selected.is_empty());
    }
}
