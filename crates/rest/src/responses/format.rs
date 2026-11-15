//! Format-aware response building.
//!
//! Provides utilities for serializing FHIR resources to JSON or XML
//! based on content negotiation.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::error::RestError;
use crate::middleware::content_type::FhirFormat;

/// Builds an HTTP response body from a `serde_json::Value` in the negotiated format.
///
/// - For JSON: serializes directly as JSON
/// - For XML: converts through a typed FHIR Resource to produce valid FHIR XML
///
/// The error is the handler's answer, not a serialization detail to wrap:
/// a build without the `xml` feature refuses XML with `406 Not Acceptable`,
/// and only a resource the typed model cannot represent is a `500`.
#[allow(clippy::result_large_err)]
pub fn format_resource_response(
    status: StatusCode,
    headers: axum::http::HeaderMap,
    content: &Value,
    format: FhirFormat,
) -> Result<Response, RestError> {
    // A handler that did not set a Content-Type (search, history, delete,
    // ...) still answers a FHIR media type: the XML string body otherwise went
    // out as `text/plain` and the JSON one as `application/json`, and HAPI's
    // generic client — the cqf CQL engine's REST retrieve provider among its
    // users — rejects `text/plain` outright.
    // Nor may a handler's Content-Type contradict the body: `ResourceHeaders`
    // defaults to `application/fhir+json`, so create / update answered FHIR XML
    // labelled as JSON. The media type follows the body; a `fhirVersion`
    // parameter the handler set is kept.
    let mut headers = headers;
    let (mime, token) = match format {
        FhirFormat::Xml => ("application/fhir+xml; charset=utf-8", "xml"),
        FhirFormat::Json | FhirFormat::NdJson => ("application/fhir+json", "json"),
    };
    let current = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase);
    let agrees = current.as_deref().is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|media| media.contains(token))
    });
    if !agrees {
        let params = current
            .as_deref()
            .and_then(|ct| ct.split_once(';').map(|(_, p)| p.trim().to_owned()))
            .filter(|p| p.starts_with("fhirversion="));
        let value = match params {
            Some(p) => format!("{mime}; {}", p.replacen("fhirversion", "fhirVersion", 1)),
            None => mime.to_owned(),
        };
        if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
            headers.insert(axum::http::header::CONTENT_TYPE, v);
        }
    }
    match format {
        // `Json` borrows: `&Value` is `Serialize`, so the body is written
        // straight out of the caller's value. Cloning here copied the whole
        // response — for a searchset that is every matched resource — purely to
        // hand `Json` an owned tree it then serializes and drops.
        FhirFormat::Json => Ok((status, headers, axum::Json(content)).into_response()),
        #[cfg(feature = "xml")]
        FhirFormat::Xml => {
            let xml = value_to_xml(content).map_err(|e| RestError::InternalError {
                message: format!("Failed to serialize to XML: {}", e),
            })?;
            // A bare `String` body is `text/plain`; say what it is unless the
            // handler already did.
            let mut headers = headers;
            if !headers.contains_key(axum::http::header::CONTENT_TYPE) {
                headers.insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/fhir+xml; charset=utf-8"),
                );
            }
            Ok((status, headers, xml).into_response())
        }
        #[cfg(not(feature = "xml"))]
        FhirFormat::Xml => Err(RestError::NotAcceptable {
            message: "XML format is not supported (xml feature not enabled)".to_string(),
        }),
        FhirFormat::NdJson => {
            // NdJson is typically for bulk operations, not single resource responses
            Ok((status, headers, axum::Json(content)).into_response())
        }
    }
}

/// Converts a `serde_json::Value` (JSON FHIR resource) to an XML string.
///
/// This dispatches through the appropriate FHIR version's typed Resource enum
/// to ensure proper XML serialization with FHIR namespace and structure.
#[cfg(feature = "xml")]
fn value_to_xml(value: &Value) -> Result<String, String> {
    use helios_fhir::FhirVersion;

    // Determine version from the resource content or default to R4
    let version = detect_fhir_version(value);

    match version {
        #[cfg(feature = "R4")]
        FhirVersion::R4 => {
            let resource: helios_fhir::r4::Resource = serde_json::from_value(value.clone())
                .map_err(|e| format!("Failed to parse as R4 Resource: {}", e))?;
            helios_serde::xml::to_xml_string(&resource)
                .map_err(|e| format!("XML serialization error: {}", e))
        }
        #[cfg(feature = "R4B")]
        FhirVersion::R4B => {
            let resource: helios_fhir::r4b::Resource = serde_json::from_value(value.clone())
                .map_err(|e| format!("Failed to parse as R4B Resource: {}", e))?;
            helios_serde::xml::to_xml_string(&resource)
                .map_err(|e| format!("XML serialization error: {}", e))
        }
        #[cfg(feature = "R5")]
        FhirVersion::R5 => {
            let resource: helios_fhir::r5::Resource = serde_json::from_value(value.clone())
                .map_err(|e| format!("Failed to parse as R5 Resource: {}", e))?;
            helios_serde::xml::to_xml_string(&resource)
                .map_err(|e| format!("XML serialization error: {}", e))
        }
        #[cfg(feature = "R6")]
        FhirVersion::R6 => {
            let resource: helios_fhir::r6::Resource = serde_json::from_value(value.clone())
                .map_err(|e| format!("Failed to parse as R6 Resource: {}", e))?;
            helios_serde::xml::to_xml_string(&resource)
                .map_err(|e| format!("XML serialization error: {}", e))
        }
        #[allow(unreachable_patterns)]
        _ => Err(format!(
            "FHIR version {:?} is not enabled in this build",
            version
        )),
    }
}

/// Converts an XML string to a `serde_json::Value` by deserializing through
/// the typed FHIR Resource enum.
#[cfg(feature = "xml")]
pub fn xml_to_value(xml: &str) -> Result<Value, String> {
    // Default to R4 for incoming XML (the specific version can be overridden).
    // Lossless, schema-driven conversion: the typed model would silently drop
    // unknown elements and attributes and reject values it cannot hold, so
    // neither validation nor storage would see what the client sent (the
    // JSON path keeps everything too).
    #[cfg(feature = "R4")]
    {
        let registry = helios_fhir_validator::packs::core_registry(helios_fhir::FhirVersion::R4);
        helios_fhir_validator::xml::xml_to_json(xml, registry.as_ref())
            .map_err(|e| format!("Failed to parse XML: {e}"))
    }
    #[cfg(not(feature = "R4"))]
    {
        Err("R4 feature not enabled for XML parsing".to_string())
    }
}

/// Detects the FHIR version from resource content.
/// Falls back to R4 if unable to determine.
#[cfg(feature = "xml")]
fn detect_fhir_version(_value: &Value) -> helios_fhir::FhirVersion {
    // Could check meta.profile or other indicators
    // For now, default to the first available version
    #[cfg(feature = "R4")]
    return helios_fhir::FhirVersion::R4;
    #[cfg(all(not(feature = "R4"), feature = "R4B"))]
    return helios_fhir::FhirVersion::R4B;
    #[cfg(all(not(feature = "R4"), not(feature = "R4B"), feature = "R5"))]
    return helios_fhir::FhirVersion::R5;
    #[cfg(all(
        not(feature = "R4"),
        not(feature = "R4B"),
        not(feature = "R5"),
        feature = "R6"
    ))]
    return helios_fhir::FhirVersion::R6;
    #[allow(unreachable_code)]
    {
        helios_fhir::FhirVersion::default()
    }
}

#[cfg(all(test, feature = "xml"))]
mod xml_content_type_tests {
    use super::*;

    #[test]
    fn an_xml_body_is_labelled_fhir_xml() {
        let bundle = serde_json::json!({"resourceType": "Bundle", "type": "searchset"});
        let response = format_resource_response(
            StatusCode::OK,
            axum::http::HeaderMap::new(),
            &bundle,
            FhirFormat::Xml,
        )
        .unwrap();
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "application/fhir+xml; charset=utf-8"
        );
    }
}
