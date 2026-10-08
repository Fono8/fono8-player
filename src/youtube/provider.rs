//! Read-only operations over the helper transport: search, liked playlists and
//! playlist import with bounded pagination.
//! Playlist editing happens in Fono8's database.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::i18n::Message;
use crate::library::Track;

use super::models::{continuation, parse_playlists, parse_tracks, playlist_id, playlist_page, playlist_title, RemotePlaylist};

const MAX_PAGES: usize = 99;
const MAX_IMPORT: usize = 10_000;

/// One in-flight operation; fed with responses until it is done.
pub enum Operation {
    Search,
    Playlists { results: Vec<RemotePlaylist>, seen: HashSet<String> },
    Import { name: String, tracks: Vec<Track>, seen: HashSet<String>, enqueue: bool },
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Tracks(Vec<Track>),
    Playlists(Vec<RemotePlaylist>),
    Imported { name: String, tracks: Vec<Track>, enqueue: bool },
    Failed(Message),
}

pub enum Step {
    /// Issue another request with this endpoint and body.
    Request(&'static str, Value),
    Done(Outcome),
}

impl Operation {
    pub fn search(query: &str) -> Result<(Operation, Step), Message> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 500 {
            return Err(Message::new("yt_invalid_query"));
        }
        Ok((Operation::Search, Step::Request("search", json!({"query": query}))))
    }

    pub fn playlists() -> (Operation, Step) {
        (
            Operation::Playlists { results: Vec::new(), seen: HashSet::new() },
            Step::Request("browse", json!({"browseId": "FEmusic_liked_playlists"})),
        )
    }

    pub fn import(identifier: &str, enqueue: bool) -> Result<(Operation, Step), Message> {
        let id = playlist_id(identifier).map_err(|_| Message::new("yt_invalid_playlist"))?;
        let step = Step::Request("browse", json!({"browseId": format!("VL{id}")}));
        Ok((Operation::Import { name: id, tracks: Vec::new(), seen: HashSet::new(), enqueue }, step))
    }

    /// Feed one response; returns the next request or the final outcome.
    /// A failed or looping import never yields a partial result.
    pub fn respond(&mut self, response: Result<Value, Message>) -> Step {
        let data = match response {
            Ok(data) => data,
            Err(message) => return Step::Done(Outcome::Failed(message)),
        };
        let failed = || Step::Done(Outcome::Failed(Message::new("yt_response_error")));
        match self {
            Operation::Search => match parse_tracks(&data) {
                Ok(tracks) => Step::Done(Outcome::Tracks(tracks)),
                Err(()) => failed(),
            },
            Operation::Playlists { results, seen } => {
                let (playlists, token) = match parse_playlists(&data).and_then(|p| Ok((p, continuation(&data)?))) {
                    Ok(parsed) => parsed,
                    Err(()) => return failed(),
                };
                for playlist in playlists {
                    if !results.iter().any(|p| p.id == playlist.id) {
                        results.push(playlist);
                    }
                }
                match next_page(token, seen) {
                    Ok(Some(token)) => Step::Request("browse", json!({"continuation": token})),
                    Ok(None) => Step::Done(Outcome::Playlists(std::mem::take(results))),
                    Err(()) => failed(),
                }
            }
            Operation::Import { name, tracks, seen, enqueue } => {
                let page = (|| -> Result<(String, Vec<Track>, Option<String>), ()> {
                    let title = playlist_title(&data, name)?;
                    let shelf = playlist_page(&data)?;
                    Ok((title, parse_tracks(shelf)?, continuation(shelf)?))
                })();
                let (title, parsed, token) = match page {
                    Ok(page) => page,
                    Err(()) => return failed(),
                };
                *name = title;
                for track in parsed {
                    if !tracks.iter().any(|t| t.path == track.path) {
                        tracks.push(track);
                    }
                }
                if tracks.len() > MAX_IMPORT {
                    return failed();
                }
                match next_page(token, seen) {
                    Ok(Some(token)) => Step::Request("browse", json!({"continuation": token})),
                    Ok(None) => {
                        Step::Done(Outcome::Imported { name: name.clone(), tracks: std::mem::take(tracks), enqueue: *enqueue })
                    }
                    Err(()) => failed(),
                }
            }
        }
    }
}

fn next_page(token: Option<String>, seen: &mut HashSet<String>) -> Result<Option<String>, ()> {
    match token {
        Some(token) => {
            if seen.contains(&token) || seen.len() >= MAX_PAGES {
                return Err(());
            }
            seen.insert(token.clone());
            Ok(Some(token))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::models::tests::{page, row, ONE, TWO};
    use super::*;

    fn drive(mut operation: Operation, first: Step, responses: Vec<Result<Value, Message>>) -> (Vec<(String, Value)>, Outcome) {
        let mut calls = Vec::new();
        let mut step = first;
        let mut responses = responses.into_iter();
        loop {
            match step {
                Step::Request(endpoint, body) => {
                    calls.push((endpoint.to_string(), body));
                    step = operation.respond(responses.next().expect("response"));
                }
                Step::Done(outcome) => return (calls, outcome),
            }
        }
    }

    #[test]
    fn import_reads_all_pages_and_ignores_suggestions() {
        let (operation, step) = Operation::import("PLabc", false).unwrap();
        let second = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [row(TWO, "Second", false), row(ONE, "Song", false)]}}]});
        let (calls, outcome) = drive(operation, step, vec![Ok(page(vec![row(ONE, "Song", false)], Some("page2"))), Ok(second)]);
        assert_eq!(
            calls,
            vec![("browse".into(), json!({"browseId": "VLPLabc"})), ("browse".into(), json!({"continuation": "page2"}))]
        );
        match outcome {
            Outcome::Imported { name, tracks, enqueue } => {
                assert_eq!(name, "My mix");
                assert_eq!(tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["Song", "Second"]);
                assert!(!enqueue);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failed_or_looping_pagination_never_returns_partial_import() {
        for second in [
            Ok(page(vec![row(TWO, "x", false)], Some("loop"))),
            Ok(json!({"unknown": "new schema"})),
            Err(Message::new("yt_auth_error")),
        ] {
            let (operation, step) = Operation::import("PLabc", false).unwrap();
            let (_, outcome) = drive(operation, step, vec![Ok(page(vec![row(ONE, "Song", false)], Some("loop"))), second]);
            assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        }
    }

    #[test]
    fn network_failure_and_empty_search() {
        let (operation, step) = Operation::search("Artist").unwrap();
        let (_, outcome) = drive(operation, step, vec![Err(Message::new("yt_network_error"))]);
        assert_eq!(outcome, Outcome::Failed(Message::new("yt_network_error")));
        let (operation, step) = Operation::search("Other").unwrap();
        let (_, outcome) = drive(operation, step, vec![Ok(json!({"contents": {}}))]);
        assert_eq!(outcome, Outcome::Tracks(vec![]));
        assert!(Operation::search("   ").is_err());
        assert!(Operation::import("../private", false).is_err());
    }

    #[test]
    fn liked_playlists_follow_continuations() {
        let (operation, step) = Operation::playlists();
        let first = json!({"items": [{"musicTwoRowItemRenderer": {"title": {"runs": [{"text": "List"}]}, "navigationEndpoint": {"browseEndpoint": {"browseId": "VLPLabc"}}}}],
            "continuations": [{"nextContinuationData": {"continuation": "more"}}]});
        let second = json!({"items": [{"musicTwoRowItemRenderer": {"title": {"runs": [{"text": "Other"}]}, "navigationEndpoint": {"browseEndpoint": {"browseId": "VLPLdef"}}}}]});
        let (calls, outcome) = drive(operation, step, vec![Ok(first), Ok(second)]);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1, json!({"continuation": "more"}));
        assert_eq!(
            outcome,
            Outcome::Playlists(vec![
                RemotePlaylist { id: "PLabc".into(), title: "List".into() },
                RemotePlaylist { id: "PLdef".into(), title: "Other".into() }
            ])
        );
    }
}
