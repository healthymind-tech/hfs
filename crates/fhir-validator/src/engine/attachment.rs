//! Attachment content checks (HL7 validator): `data` must be valid base64,
//! and a stated `size` / `hash` (SHA-1, base64) must match the decoded
//! data.

use super::errors::ErrorKind;
use super::walk::WalkCtx;
use base64::Engine;
use serde_json::Value;
use sha1::{Digest, Sha1};

/// Check an Attachment's `data` against its `size` and `hash`.
pub(super) fn check_attachment(ctx: &mut WalkCtx<'_>, attachment: &serde_json::Map<String, Value>) {
    let Some(data) = attachment.get("data").and_then(Value::as_str) else {
        return;
    };
    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    let decoded = match base64::engine::general_purpose::STANDARD.decode(compact.as_bytes()) {
        Ok(bytes) => bytes,
        Err(e) => {
            // Characters outside the alphabet are the base64Binary
            // primitive's own (regex) error; malformed padding or length
            // inside the alphabet is reported here.
            let alphabet = compact
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='));
            if alphabet {
                ctx.error(
                    ErrorKind::PrimitiveValue,
                    format!("The base64 content in the data element is not valid base64: {e}"),
                );
            }
            return;
        }
    };
    if let Some(size) = attachment.get("size").and_then(Value::as_u64)
        && size != decoded.len() as u64
    {
        ctx.error(
            ErrorKind::PrimitiveValue,
            format!(
                "Stated Attachment Size {size} does not match actual attachment size {}",
                decoded.len()
            ),
        );
    }
    if let Some(hash) = attachment.get("hash").and_then(Value::as_str) {
        let actual = base64::engine::general_purpose::STANDARD.encode(Sha1::digest(&decoded));
        if hash != actual {
            ctx.error(
                ErrorKind::PrimitiveValue,
                format!(
                    "The hash of the data did not match the data (stated: '{hash}', actual: '{actual}')"
                ),
            );
        }
    }
}
