//! Sealing secret inputs into a spec, and re-sealing (`$rewrap`).
//!
//! A secret is sealed with the active key of the [`SecretKeyRing`]
//! (`HFS_SECRET_KEYS`) and bound by AAD to `endpoint:{id}:{field}` — the
//! endpoint's immutable id, so an envelope copied into another endpoint or
//! field does not open. Without a key ring, setting a secret is refused
//! (`409`); keeping or clearing one still works.

use helios_endpoints::model::EndpointSpec;
use helios_endpoints::secret::SecretError;
use helios_endpoints::{SealedSecret, SecretInput, SecretKeyRing};

use super::error::{ApiError, ApiResult};
use super::spec_input::SecretInputs;

/// AAD of an endpoint secret.
pub fn aad(endpoint_id: &str, field: &str) -> String {
    format!("endpoint:{endpoint_id}:{field}")
}

/// Whether any input would store a new secret.
pub fn sets_any(inputs: &SecretInputs) -> bool {
    inputs.values().any(|i| matches!(i, SecretInput::Set(_)))
}

fn slot<'a>(spec: &'a mut EndpointSpec, field: &str) -> ApiResult<&'a mut Option<SealedSecret>> {
    match field {
        "terminology.token" => match spec.terminology.as_mut() {
            Some(t) => Ok(&mut t.token),
            None => Err(ApiError::invalid_field(
                "secrets.terminology.token",
                "the endpoint has no terminology section (it inherits the server's); set \"terminology\" first",
            )),
        },
        other => Err(ApiError::invalid_field(
            format!("secrets.{other}"),
            "not a secret field",
        )),
    }
}

/// Applies `inputs` to `spec` for endpoint `endpoint_id`.
pub fn apply(
    spec: &mut EndpointSpec,
    endpoint_id: &str,
    inputs: &SecretInputs,
    ring: Option<&SecretKeyRing>,
) -> ApiResult<()> {
    for (field, input) in inputs {
        if matches!(input, SecretInput::Keep) {
            continue;
        }
        if matches!(input, SecretInput::Clear)
            && field == "terminology.token"
            && spec.terminology.is_none()
        {
            // Nothing to clear: the section (and its secret) is gone anyway.
            continue;
        }
        let place = slot(spec, field)?;
        let current = place.take();
        *place = input.apply(current, ring, &aad(endpoint_id, field))?;
    }
    Ok(())
}

/// Every sealed secret of a spec with its field path.
pub fn sealed_fields(spec: &EndpointSpec) -> Vec<(&'static str, &SealedSecret)> {
    let mut out = Vec::new();
    if let Some(token) = spec.terminology.as_ref().and_then(|t| t.token.as_ref()) {
        out.push(("terminology.token", token));
    }
    out
}

/// Outcome of re-sealing one spec.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rewrapped {
    /// Fields re-sealed under the active key.
    pub rewrapped: Vec<String>,
    /// Fields already under the active key.
    pub current: usize,
    /// Fields that did not open (`field: reason`).
    pub failed: Vec<String>,
}

/// Re-seals every secret of `spec` not sealed with the active key.
pub fn rewrap(spec: &mut EndpointSpec, endpoint_id: &str, ring: &SecretKeyRing) -> Rewrapped {
    let mut out = Rewrapped::default();
    if let Some(t) = spec.terminology.as_mut()
        && let Some(sealed) = t.token.as_ref()
    {
        let field = "terminology.token";
        if !ring.needs_rewrap(sealed) {
            out.current += 1;
        } else {
            match ring.rewrap(sealed, &aad(endpoint_id, field)) {
                Ok(next) => {
                    t.token = Some(next);
                    out.rewrapped.push(field.to_string());
                }
                Err(e) => out.failed.push(format!("{field}: {e}")),
            }
        }
    }
    out
}

/// `409` naming the missing key ring (for the `$rewrap` route).
pub fn no_key_ring() -> ApiError {
    SecretError::NoKeyRing.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::EndpointName;
    use helios_endpoints::model::TerminologyBinding;

    fn ring(id: &str) -> SecretKeyRing {
        SecretKeyRing::parse(&SecretKeyRing::generate_spec(id)).unwrap()
    }

    fn spec_with_tx() -> EndpointSpec {
        let mut s = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        s.terminology = Some(TerminologyBinding {
            server_url: Some("https://tx.example.org".into()),
            ..Default::default()
        });
        s
    }

    fn inputs(field: &str, i: SecretInput) -> SecretInputs {
        let mut m = SecretInputs::new();
        m.insert(field.to_string(), i);
        m
    }

    #[test]
    fn set_seals_bound_to_endpoint_and_field() {
        let r = ring("k1");
        let mut s = spec_with_tx();
        let i = inputs("terminology.token", SecretInput::Set("tok".into()));
        assert!(sets_any(&i));
        apply(&mut s, "id-1", &i, Some(&r)).unwrap();
        let sealed = s.terminology.as_ref().unwrap().token.clone().unwrap();
        assert_eq!(
            r.open(&sealed, &aad("id-1", "terminology.token"))
                .unwrap()
                .expose_str(),
            Some("tok")
        );
        assert!(r.open(&sealed, &aad("id-2", "terminology.token")).is_err());
        assert_eq!(sealed_fields(&s).len(), 1);
        // Keep leaves it, clear removes it.
        apply(
            &mut s,
            "id-1",
            &inputs("terminology.token", SecretInput::Keep),
            Some(&r),
        )
        .unwrap();
        assert!(s.terminology.as_ref().unwrap().token.is_some());
        apply(
            &mut s,
            "id-1",
            &inputs("terminology.token", SecretInput::Clear),
            None,
        )
        .unwrap();
        assert!(s.terminology.as_ref().unwrap().token.is_none());
    }

    #[test]
    fn set_without_key_ring_or_section_is_refused() {
        let mut s = spec_with_tx();
        let e = apply(
            &mut s,
            "id",
            &inputs("terminology.token", SecretInput::Set("x".into())),
            None,
        )
        .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::CONFLICT);
        let mut s = EndpointSpec::named(EndpointName::parse("lab").unwrap(), "Lab");
        let e = apply(
            &mut s,
            "id",
            &inputs("terminology.token", SecretInput::Set("x".into())),
            Some(&ring("k")),
        )
        .unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        // Clearing a secret of a missing section is a no-op.
        apply(
            &mut s,
            "id",
            &inputs("terminology.token", SecretInput::Clear),
            None,
        )
        .unwrap();
        let e = apply(&mut s, "id", &inputs("other", SecretInput::Clear), None).unwrap_err();
        assert_eq!(e.issues[0].field.as_deref(), Some("secrets.other"));
        assert_eq!(no_key_ring().status, axum::http::StatusCode::CONFLICT);
    }

    #[test]
    fn rewrap_moves_to_the_active_key() {
        let old = ring("old");
        let mut s = spec_with_tx();
        apply(
            &mut s,
            "id",
            &inputs("terminology.token", SecretInput::Set("tok".into())),
            Some(&old),
        )
        .unwrap();
        // A ring without the old key cannot open it.
        let new_only = ring("new");
        let out = rewrap(&mut s, "id", &new_only);
        assert_eq!(out.failed.len(), 1, "{out:?}");
        assert!(out.rewrapped.is_empty());
        // With the old key in the ring the secret is re-sealed under the active key.
        let spec_old = SecretKeyRing::generate_spec("o2");
        let o2 = SecretKeyRing::parse(&spec_old).unwrap();
        let mut s = spec_with_tx();
        apply(
            &mut s,
            "id",
            &inputs("terminology.token", SecretInput::Set("tok".into())),
            Some(&o2),
        )
        .unwrap();
        let rotated = SecretKeyRing::parse(&format!(
            "{},{}",
            SecretKeyRing::generate_spec("n2"),
            spec_old
        ))
        .unwrap();
        let out = rewrap(&mut s, "id", &rotated);
        assert_eq!(out.rewrapped, vec!["terminology.token"]);
        let sealed = s.terminology.as_ref().unwrap().token.clone().unwrap();
        assert_eq!(sealed.key_id(), "n2");
        assert_eq!(
            rotated
                .open(&sealed, &aad("id", "terminology.token"))
                .unwrap()
                .expose_str(),
            Some("tok")
        );
        let out = rewrap(&mut s, "id", &rotated);
        assert_eq!(out.current, 1);
    }
}
