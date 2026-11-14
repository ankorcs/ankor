//! The message types a newer peer sent that this agent does not know, so that each is logged once.

use std::collections::HashSet;

use crate::failure::truncate;

/// Types remembered before the agent stops logging new ones. A newer release adds a few types, so
/// a peer past this many is misbehaving, and remembering more would only grow memory.
const MAX_REMEMBERED: usize = 32;

/// Characters of a type a log line keeps.
const MAX_TYPE_CHARS: u64 = 64;

#[derive(Debug, Default)]
pub struct SkippedTypes(HashSet<String>);

impl SkippedTypes {
    /// The type as a log line names it, cut to `MAX_TYPE_CHARS`, the first time it is seen. `None`
    /// for a type seen before, and for every new one once `MAX_REMEMBERED` are held.
    pub fn first_sighting(&mut self, kind: &str) -> Option<String> {
        let kind = truncate(kind, MAX_TYPE_CHARS);

        (self.0.len() < MAX_REMEMBERED && self.0.insert(kind.clone())).then_some(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_type_once_cut_to_length_and_stops_remembering_past_the_cap() {
        let mut skipped = SkippedTypes::default();

        assert_eq!(skipped.first_sighting("node_weather").as_deref(), Some("node_weather"));
        assert_eq!(skipped.first_sighting("node_weather"), None);
        assert_eq!(skipped.first_sighting(&"x".repeat(200)).map(|kind| kind.chars().count()), Some(64));

        for index in 0..100 {
            let _ = skipped.first_sighting(&format!("type_{index}"));
        }

        assert_eq!(skipped.0.len(), MAX_REMEMBERED);
        assert_eq!(skipped.first_sighting("one_more"), None);
    }
}
