//! Where refresh tokens live between runs: the system keyring (Secret Service
//! on Linux, Keychain on macOS, Credential Manager on Windows), one entry per
//! service and Client ID. A saved token may be stale (Spotify's expire after
//! six months).

const SERVICE: &str = "fono8";

pub trait TokenStore: Send {
    fn load(&self, client_id: &str) -> Option<String>;
    /// `false` when the keyring is unavailable; the session then lasts until Fono8 quits.
    fn save(&self, client_id: &str, token: &str) -> bool;
    fn delete(&self, client_id: &str);
}

/// Keyring entries named `<service>:<client id>` (e.g. `spotify:81303e…`).
pub struct Keyring {
    pub service: &'static str,
}

impl Keyring {
    fn account(&self, client_id: &str) -> String {
        format!("{}:{client_id}", self.service)
    }
}

impl TokenStore for Keyring {
    fn load(&self, client_id: &str) -> Option<String> {
        keyring::Entry::new(SERVICE, &self.account(client_id)).ok()?.get_password().ok().filter(|t| !t.is_empty())
    }

    fn save(&self, client_id: &str, token: &str) -> bool {
        keyring::Entry::new(SERVICE, &self.account(client_id)).and_then(|entry| entry.set_password(token)).is_ok()
    }

    fn delete(&self, client_id: &str) {
        if let Ok(entry) = keyring::Entry::new(SERVICE, &self.account(client_id)) {
            let _ = entry.delete_credential();
        }
    }
}

/// In-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory {
    pub tokens: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub unavailable: bool,
}

#[cfg(test)]
impl TokenStore for Memory {
    fn load(&self, client_id: &str) -> Option<String> {
        self.tokens.lock().ok()?.get(client_id).cloned()
    }

    fn save(&self, client_id: &str, token: &str) -> bool {
        if self.unavailable {
            return false;
        }
        self.tokens.lock().map(|mut t| t.insert(client_id.to_string(), token.to_string())).is_ok()
    }

    fn delete(&self, client_id: &str) {
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.remove(client_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through the real keyring: `cargo test keyring -- --ignored`.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let client = "0123456789abcdef0123456789abcdef";
        let store = Keyring { service: "fono8-test" };
        assert!(store.save(client, "fono8-test-token"), "keyring unavailable");
        assert_eq!(store.load(client).as_deref(), Some("fono8-test-token"));
        store.delete(client);
        assert_eq!(store.load(client), None);
    }
}
