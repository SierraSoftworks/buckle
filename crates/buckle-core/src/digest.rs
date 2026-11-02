//! Content digests. Buckle identifies revisions, blobs, plans and task specs by
//! SHA-256; the short form is what operators see.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fmt;

/// A SHA-256 digest, hex encoded.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Hashes a byte string.
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// Hashes the canonical JSON serialisation of a value, so that structurally
    /// equal values share a digest regardless of field order in the source.
    pub fn of_json<T: Serialize>(value: &T) -> Self {
        let canonical = canonical_json(value);
        Self::of(canonical.as_bytes())
    }

    /// Wraps an already-hex-encoded digest.
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The full hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first eight hex characters, for display.
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(8)]
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Serialises `value` as JSON with object keys sorted, recursively.
pub fn canonical_json<T: Serialize>(value: &T) -> String {
    let v = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    sort_keys(v).to_string()
}

fn sort_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(String, serde_json::Value)> = map.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, sort_keys(v));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sort_keys).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn digest_of_bytes_is_sha256_hex() {
        assert_eq!(
            Digest::of(b"hello").as_str(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(Digest::of(b"hello").short(), "2cf24dba");
    }

    #[rstest]
    #[case(serde_json::json!({"b": 1, "a": {"d": 2, "c": 3}}), serde_json::json!({"a": {"c": 3, "d": 2}, "b": 1}))]
    #[case(serde_json::json!([{"z": 1, "y": 2}]), serde_json::json!([{"y": 2, "z": 1}]))]
    fn json_digests_ignore_key_order(#[case] a: serde_json::Value, #[case] b: serde_json::Value) {
        assert_eq!(Digest::of_json(&a), Digest::of_json(&b));
    }
}
