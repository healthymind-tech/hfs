//! Content hashing for idempotent writes (design §9.2 "Idempotency").
//!
//! The hash covers the resource as the source holds it, minus the two
//! fields every server rewrites on write (`meta.versionId`,
//! `meta.lastUpdated`), serialized canonically (object keys sorted,
//! independent of `serde_json`'s `preserve_order` feature). Two reads of an
//! unchanged resource therefore hash equal on any server, and a resource that
//! only received a new version stamp is not written again.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 of the canonical form of `resource`.
pub fn content_hash(resource: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    write_canonical(&strip_volatile(resource), &mut |b| hasher.update(b));
    hasher.finalize().into()
}

/// A copy of `resource` without `meta.versionId` / `meta.lastUpdated`
/// (and without `meta` when nothing else is left in it).
pub fn strip_volatile(resource: &Value) -> Value {
    let mut v = resource.clone();
    if let Some(obj) = v.as_object_mut() {
        let drop_meta = match obj.get_mut("meta").and_then(Value::as_object_mut) {
            Some(meta) => {
                meta.remove("versionId");
                meta.remove("lastUpdated");
                meta.is_empty()
            }
            None => false,
        };
        if drop_meta {
            obj.remove("meta");
        }
    }
    v
}

/// Canonical JSON text (sorted keys, no whitespace).
pub fn canonical_string(v: &Value) -> String {
    let mut out = Vec::new();
    write_canonical(v, &mut |b| out.extend_from_slice(b));
    String::from_utf8(out).expect("JSON text is UTF-8")
}

fn write_canonical(v: &Value, out: &mut dyn FnMut(&[u8])) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out(b"{");
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out(b",");
                }
                out(serde_json::to_string(k).expect("string").as_bytes());
                out(b":");
                write_canonical(&map[k], out);
            }
            out(b"}");
        }
        Value::Array(items) => {
            out(b"[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out(b",");
                }
                write_canonical(item, out);
            }
            out(b"]");
        }
        other => out(serde_json::to_string(other).expect("scalar").as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignores_version_stamp_and_key_order() {
        let a = json!({"resourceType": "Patient", "id": "p1",
                       "meta": {"versionId": "1", "lastUpdated": "2020-01-01T00:00:00Z"},
                       "name": [{"family": "Chen", "given": ["Mei"]}]});
        let b = json!({"name": [{"given": ["Mei"], "family": "Chen"}], "id": "p1",
                       "meta": {"versionId": "7", "lastUpdated": "2026-01-01T00:00:00Z"},
                       "resourceType": "Patient"});
        assert_eq!(content_hash(&a), content_hash(&b));
        let no_meta = json!({"resourceType": "Patient", "id": "p1",
                             "name": [{"family": "Chen", "given": ["Mei"]}]});
        assert_eq!(content_hash(&a), content_hash(&no_meta));
    }

    #[test]
    fn content_changes_change_the_hash() {
        let a = json!({"resourceType": "Patient", "id": "p1", "active": true});
        let b = json!({"resourceType": "Patient", "id": "p1", "active": false});
        assert_ne!(content_hash(&a), content_hash(&b));
        // Other meta fields (tags, profiles) are content.
        let t = json!({"resourceType": "Patient", "id": "p1", "active": true,
                       "meta": {"tag": [{"code": "x"}]}});
        assert_ne!(content_hash(&a), content_hash(&t));
        // Array order is content.
        let x = json!({"a": [1, 2]});
        let y = json!({"a": [2, 1]});
        assert_ne!(content_hash(&x), content_hash(&y));
    }

    #[test]
    fn canonical_text() {
        assert_eq!(
            canonical_string(&json!({"b": 1, "a": {"d": [true, null], "c": "x\"y"}})),
            r#"{"a":{"c":"x\"y","d":[true,null]},"b":1}"#
        );
        assert_eq!(strip_volatile(&json!("s")), json!("s"));
    }
}
