//! Serde pieces the generated types use.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A unit type that reads and writes one fixed string.
#[macro_export]
macro_rules! literal {
    ($name:ident, $text:literal) => {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub struct $name;

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str($text)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;

                if text == $text { Ok(Self) } else { Err(serde::de::Error::custom(concat!("expected ", stringify!($text)))) }
            }
        }
    };
}

/// `value` with `tag` added under `key`.
#[must_use]
pub fn with_tag(key: &str, tag: Value, value: Value) -> Value {
    match value {
        Value::Object(mut fields) => {
            fields.insert(key.to_owned(), tag);
            Value::Object(fields)
        }
        other => other,
    }
}

/// An optional nullable field: `None` when absent, `Some(None)` when null.
#[allow(clippy::option_option, reason = "absent and null are different values on the wire")]
pub mod double_option {
    use super::{Deserialize, Deserializer, Serialize, Serializer};

    /// # Errors
    ///
    /// Fails as the inner value's serializer fails.
    #[allow(clippy::ref_option, reason = "serde passes the field by reference")]
    pub fn serialize<T: Serialize, S: Serializer>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    /// # Errors
    ///
    /// Fails as the inner value's deserializer fails.
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn tags_go_on_objects_only() {
        assert_eq!(with_tag("type", json!("hello"), json!({ "a": 1 })), json!({ "a": 1, "type": "hello" }));
        assert_eq!(with_tag("type", json!("hello"), json!([1])), json!([1]));
    }

    #[test]
    fn null_reads_as_some_none_and_a_value_as_some_some() {
        assert_eq!(double_option::deserialize::<u32, _>(json!(5)).ok(), Some(Some(Some(5))));
        assert_eq!(double_option::deserialize::<u32, _>(Value::Null).ok(), Some(Some(None)));
    }
}
