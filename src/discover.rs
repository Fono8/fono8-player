//! The Discover page: one search over every connected streaming service, with
//! per-service tabs for playlists, liked songs and imports. Each service keeps
//! its own results; this module mixes them into one list and owns the
//! selection across services.

use crate::services::Service;

/// One visible row: the service and the index in that service's results.
pub type RowRef = (Service, usize);

#[derive(Default)]
pub struct Discover {
    /// `None` shows every service mixed; `Some` shows one service and its tools.
    pub filter: Option<Service>,
    selected: Vec<RowRef>,
    anchor: Option<RowRef>,
    /// Target playlist for "Add selected"; `None` means the whole library.
    pub target: Option<i64>,
}

/// Rows of several services, alternating by rank so no service dominates the top.
pub fn interleave(lists: &[(Service, usize)]) -> Vec<RowRef> {
    let longest = lists.iter().map(|(_, len)| *len).max().unwrap_or(0);
    let mut rows = Vec::with_capacity(lists.iter().map(|(_, len)| len).sum());
    for rank in 0..longest {
        for (service, len) in lists {
            if rank < *len {
                rows.push((*service, rank));
            }
        }
    }
    rows
}

impl Discover {
    pub fn is_selected(&self, row: RowRef) -> bool {
        self.selected.contains(&row)
    }

    pub fn has_selection(&self) -> bool {
        !self.selected.is_empty()
    }

    /// Indices selected in one service, in result order.
    pub fn selected_for(&self, service: Service) -> Vec<usize> {
        let mut indices: Vec<usize> = self.selected.iter().filter(|(s, _)| *s == service).map(|(_, i)| *i).collect();
        indices.sort_unstable();
        indices
    }

    pub fn services_selected(&self) -> Vec<Service> {
        Service::ALL.into_iter().filter(|s| self.selected.iter().any(|(selected, _)| selected == s)).collect()
    }

    /// The results of `service` changed: its old selection no longer applies.
    pub fn clear_service(&mut self, service: Service) {
        self.selected.retain(|(s, _)| *s != service);
        if self.anchor.is_some_and(|(s, _)| s == service) {
            self.anchor = None;
        }
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Click on the row at `position` of `visible`, with Ctrl/Shift like the track list.
    pub fn select(&mut self, visible: &[RowRef], position: usize, control: bool, shift: bool) {
        let Some(row) = visible.get(position).copied() else { return };
        if shift {
            let anchor = self.anchor.and_then(|a| visible.iter().position(|r| *r == a)).unwrap_or(position);
            if !control {
                self.selected.clear();
            }
            for row in &visible[anchor.min(position)..=anchor.max(position)] {
                if !self.selected.contains(row) {
                    self.selected.push(*row);
                }
            }
        } else if control {
            match self.selected.iter().position(|r| *r == row) {
                Some(index) => {
                    self.selected.remove(index);
                }
                None => self.selected.push(row),
            }
            self.anchor = Some(row);
        } else {
            self.selected = vec![row];
            self.anchor = Some(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaves_by_rank() {
        let rows = interleave(&[(Service::Spotify, 3), (Service::YouTube, 1)]);
        assert_eq!(rows, [(Service::Spotify, 0), (Service::YouTube, 0), (Service::Spotify, 1), (Service::Spotify, 2)]);
        assert!(interleave(&[]).is_empty());
    }

    #[test]
    fn selection_spans_services_and_survives_other_service_updates() {
        let visible = interleave(&[(Service::Spotify, 2), (Service::YouTube, 2)]);
        let mut discover = Discover::default();
        discover.select(&visible, 0, false, false);
        discover.select(&visible, 2, false, true);
        assert_eq!(discover.selected_for(Service::Spotify), [0, 1]);
        assert_eq!(discover.selected_for(Service::YouTube), [0]);
        assert_eq!(discover.services_selected(), [Service::YouTube, Service::Spotify]);
        discover.select(&visible, 1, true, false);
        assert!(discover.selected_for(Service::YouTube).is_empty());
        discover.clear_service(Service::Spotify);
        assert!(!discover.has_selection());
        discover.select(&visible, 3, false, false);
        assert!(discover.is_selected((Service::YouTube, 1)));
    }
}
