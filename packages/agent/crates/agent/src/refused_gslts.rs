//! Tokens Steam refused, kept across restarts, because Steam never takes a refused token back.
//! The file holds each token's SHA-256, never the token.

use indexmap::IndexSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::write_atomic;
use crate::logger::Logger;

#[derive(Serialize, Deserialize)]
struct RefusedFile {
    refused: Vec<String>,
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub struct RefusedGslts {
    path: Option<PathBuf>,
    refused: IndexSet<String>,
    log: Logger,
}

impl RefusedGslts {
    /// Without a path, refusals last as long as the process. An unreadable file is logged, and its
    /// tokens are checked with Steam again.
    #[must_use]
    pub fn new(path: Option<PathBuf>, log: Logger) -> Self {
        let refused = path.as_ref().map(|path| Self::read(path, &log)).unwrap_or_default();

        Self { path, refused, log }
    }

    fn read(path: &PathBuf, log: &Logger) -> IndexSet<String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return IndexSet::new(),
            Err(error) => {
                log.warn(format!("could not read {}: {error}; refused tokens are checked again", path.display()));
                return IndexSet::new();
            }
        };

        match serde_json::from_str::<RefusedFile>(&text) {
            Ok(file) => file.refused.into_iter().collect(),
            Err(error) => {
                log.warn(format!("{} is not a list of refused tokens ({error}); refused tokens are checked again", path.display()));
                IndexSet::new()
            }
        }
    }

    #[must_use]
    pub fn has(&self, token: &str) -> bool {
        self.refused.contains(&digest(token))
    }

    /// A failed write keeps the refusal in memory, so after a restart the token is checked once more.
    pub fn add(&mut self, token: &str) {
        if !self.refused.insert(digest(token)) {
            return;
        }

        let Some(path) = &self.path else {
            return;
        };
        let file = RefusedFile { refused: self.refused.iter().cloned().collect() };
        let text = format!("{}\n", serde_json::to_string_pretty(&file).unwrap_or_default());

        if let Err(error) = write_atomic(path, &text) {
            self.log.warn(format!("could not record a refused token in {}: {error}", path.display()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test locks its fixtures and fails on a poisoned lock")]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::testing::fixtures::{GSLT_A, GSLT_B};

    fn recording() -> (Logger, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&lines);

        (Logger::new("test", Some(Arc::new(move |entry: crate::logger::Entry| seen.lock().unwrap().push(entry.message)))), lines)
    }

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("gslt-refused.json")
    }

    #[test]
    fn remembers_a_refusal_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();

        RefusedGslts::new(Some(file(&dir)), Logger::null()).add(GSLT_B);

        let restarted = RefusedGslts::new(Some(file(&dir)), Logger::null());

        assert!(restarted.has(GSLT_B));
        assert!(!restarted.has(GSLT_A));
    }

    #[test]
    fn writes_each_tokens_hash_never_the_token_and_each_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut refused = RefusedGslts::new(Some(file(&dir)), Logger::null());

        refused.add(GSLT_B);
        refused.add(GSLT_B);
        refused.add(GSLT_A);

        let text = std::fs::read_to_string(file(&dir)).unwrap();

        assert!(!text.contains(GSLT_A) && !text.contains(GSLT_B));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap(), serde_json::json!({ "refused": [digest(GSLT_B), digest(GSLT_A)] }));
    }

    #[test]
    fn keeps_refusals_in_memory_without_a_path_touching_no_file() {
        let (log, lines) = recording();
        let mut refused = RefusedGslts::new(None, log);

        refused.add(GSLT_B);
        assert!(refused.has(GSLT_B));
        assert!(!refused.has(GSLT_A));
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn starts_empty_and_quietly_without_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (log, lines) = recording();

        assert!(!RefusedGslts::new(Some(file(&dir)), log).has(GSLT_A));
        assert!(lines.lock().unwrap().is_empty());
    }

    #[test]
    fn starts_empty_from_a_file_it_cannot_use_and_says_so() {
        for contents in ["{", r#"{"refused":[1]}"#] {
            let dir = tempfile::tempdir().unwrap();
            let (log, lines) = recording();

            std::fs::write(file(&dir), contents).unwrap();

            let mut refused = RefusedGslts::new(Some(file(&dir)), log);

            assert!(!refused.has(GSLT_A));
            assert!(lines.lock().unwrap()[0].contains(&format!("{} is not a list of refused tokens", file(&dir).display())), "{contents}");
            refused.add(GSLT_B);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(file(&dir)).unwrap()).unwrap(),
                serde_json::json!({ "refused": [digest(GSLT_B)] })
            );
        }
    }

    #[test]
    fn starts_empty_from_a_file_it_cannot_read_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (log, lines) = recording();

        std::fs::create_dir(file(&dir)).unwrap();

        assert!(!RefusedGslts::new(Some(file(&dir)), log).has(GSLT_A));
        assert!(lines.lock().unwrap()[0].contains(&format!("could not read {}", file(&dir).display())));
    }

    #[test]
    fn keeps_a_refusal_it_could_not_write_and_says_so_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("gslt-refused.json");
        let (log, lines) = recording();
        let mut refused = RefusedGslts::new(Some(path.clone()), log);

        refused.add(GSLT_B);
        refused.add(GSLT_B);

        assert!(refused.has(GSLT_B));

        let lines = lines.lock().unwrap();

        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&format!("could not record a refused token in {}", path.display())));
    }
}
