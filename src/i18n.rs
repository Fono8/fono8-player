//! Packaged message catalogs with system locale selection and English fallback.
//!
//! The catalogs are JSON files (`locales/*.json`) embedded at compile time. Keys are stable identifiers; plural forms are
//! objects with `one`/`few`/`many`/`other` entries.

use std::collections::HashMap;

use serde_json::Value;

pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("pl", "Polski")];

const EN: &str = include_str!("../locales/en.json");
const PL: &str = include_str!("../locales/pl.json");

/// A deferred, re-translatable message: a catalog key plus named parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub key: &'static str,
    pub values: Vec<(&'static str, Arg)>,
}

/// A message parameter: either literal text or another message.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Text(String),
    Count(i64),
    Message(Box<Message>),
}

impl From<String> for Arg {
    fn from(value: String) -> Self {
        Arg::Text(value)
    }
}

impl From<&str> for Arg {
    fn from(value: &str) -> Self {
        Arg::Text(value.to_string())
    }
}

impl From<i64> for Arg {
    fn from(value: i64) -> Self {
        Arg::Count(value)
    }
}

impl From<usize> for Arg {
    fn from(value: usize) -> Self {
        Arg::Count(value as i64)
    }
}

impl From<Message> for Arg {
    fn from(value: Message) -> Self {
        Arg::Message(Box::new(value))
    }
}

impl Message {
    pub fn new(key: &'static str) -> Self {
        Self { key, values: Vec::new() }
    }

    pub fn with(mut self, name: &'static str, value: impl Into<Arg>) -> Self {
        self.values.push((name, value.into()));
        self
    }

    pub fn count(key: &'static str, n: usize) -> Self {
        Self::new(key).with("n", n)
    }
}

pub fn system_language() -> &'static str {
    let candidates: Vec<String> = std::env::var("LANGUAGE")
        .ok()
        .into_iter()
        .flat_map(|value| value.split(':').map(str::to_string).collect::<Vec<_>>())
        .chain(sys_locale::get_locales())
        .chain(["LC_ALL", "LC_MESSAGES", "LANG"].iter().filter_map(|name| std::env::var(name).ok()))
        .collect();
    for name in candidates {
        let code = name.replace('_', "-");
        let code = code.split('-').next().unwrap_or("").split('.').next().unwrap_or("").to_lowercase();
        if let Some((known, _)) = LANGUAGES.iter().find(|(known, _)| *known == code) {
            return known;
        }
    }
    "en"
}

pub struct Translator {
    catalogs: HashMap<&'static str, HashMap<String, Value>>,
    pub preference: String,
    pub language: &'static str,
}

impl Translator {
    pub fn new(preference: &str) -> Self {
        let mut catalogs = HashMap::new();
        for (code, text) in [("en", EN), ("pl", PL)] {
            let parsed: HashMap<String, Value> = serde_json::from_str(text).expect("embedded locale catalog must be valid JSON");
            catalogs.insert(code, parsed);
        }
        let mut translator = Self { catalogs, preference: "auto".into(), language: "en" };
        translator.set_language(preference);
        translator
    }

    pub fn set_language(&mut self, preference: &str) {
        let known = LANGUAGES.iter().find(|(code, _)| *code == preference).map(|(code, _)| *code);
        self.preference = if preference == "auto" || known.is_some() { preference.to_string() } else { "auto".into() };
        self.language = match known {
            Some(code) if self.preference != "auto" => code,
            _ => system_language(),
        };
    }

    /// Plain text for a key without parameters.
    pub fn t(&self, key: &str) -> String {
        self.text(key, &[])
    }

    /// Text for a key, substituting `{name}` placeholders.
    pub fn text(&self, key: &str, values: &[(&str, Arg)]) -> String {
        let catalog = &self.catalogs[self.language];
        let in_catalog = catalog.contains_key(key);
        let template = catalog.get(key).or_else(|| self.catalogs["en"].get(key));
        let template: String = match template {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Object(forms)) => {
                let n = values
                    .iter()
                    .find(|(name, _)| *name == "n")
                    .and_then(|(_, value)| match value {
                        Arg::Count(n) => Some(*n),
                        Arg::Text(text) => text.parse().ok(),
                        _ => None,
                    })
                    .unwrap_or(0);
                let category = if self.language == "pl" && in_catalog {
                    if n == 1 {
                        "one"
                    } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
                        "few"
                    } else {
                        "many"
                    }
                } else if n == 1 {
                    "one"
                } else {
                    "other"
                };
                forms.get(category).and_then(Value::as_str).unwrap_or(key).to_string()
            }
            _ => key.to_string(),
        };
        let mut result = template;
        for (name, value) in values {
            let placeholder = format!("{{{name}}}");
            if result.contains(&placeholder) {
                result = result.replace(&placeholder, &self.render(value));
            }
        }
        result
    }

    pub fn render(&self, value: &Arg) -> String {
        match value {
            Arg::Text(text) => text.clone(),
            Arg::Count(n) => n.to_string(),
            Arg::Message(message) => self.message(message),
        }
    }

    pub fn message(&self, message: &Message) -> String {
        self.text(message.key, &message.values)
    }

    /// Older Fono8 versions persisted a placeholder instead of an empty artist tag.
    pub fn artist(&self, value: &str) -> String {
        if value.is_empty() || value == "Nieznany wykonawca" {
            self.t("unknown_artist")
        } else {
            value.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polish_plurals() {
        let t = Translator::new("pl");
        assert_eq!(t.message(&Message::count("tracks_count", 1)), "1 utwór");
        assert_eq!(t.message(&Message::count("tracks_count", 3)), "3 utwory");
        assert_eq!(t.message(&Message::count("tracks_count", 12)), "12 utworów");
        assert_eq!(t.message(&Message::count("tracks_count", 22)), "22 utwory");
    }

    #[test]
    fn english_fallback_and_parameters() {
        let t = Translator::new("en");
        assert_eq!(t.message(&Message::count("tracks_count", 2)), "2 tracks");
        let message = Message::new("track_summary").with("tracks", Message::count("tracks_count", 1)).with("duration", "3:05");
        assert_eq!(t.message(&message), "1 track · 3:05 total");
    }

    #[test]
    fn catalogs_share_keys() {
        let t = Translator::new("en");
        let en: Vec<_> = t.catalogs["en"].keys().collect();
        for key in en {
            assert!(t.catalogs["pl"].contains_key(key), "missing pl key {key}");
        }
    }
}
