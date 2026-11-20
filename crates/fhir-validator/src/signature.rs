//! Digital signature verification of `Bundle.signature` (FHIR R4 §2.36
//! Bundle, "Signing a Bundle"; Signature datatype; JSON canonicalization).
//!
//! Follows the HL7 Java validator (`BundleValidator.validateSignature` /
//! `validateSignatureJose`, message texts from its `Messages.properties`):
//!
//! - `sigFormat = application/jose`: `data` is the base64 of a JWS compact
//!   serialization (`header.payload.signature`). A detached JWS (empty
//!   payload) is expected; a payload draws a warning. The signed content is
//!   the Bundle in FHIR canonical JSON (`canonicalization=…/json`, or
//!   `…/json#document`, which also drops `Bundle.id` and `Bundle.meta`),
//!   always without `Bundle.signature`. The canonical form is the Java
//!   validator's `JsonCreatorCanonical`: object members sorted by name, no
//!   whitespace, decimals as ECMAScript numbers, strings escaped as
//!   `Utilities.escapeJson`.
//! - The verification key is the first `x5c` certificate in the JWS header,
//!   or — for a `kid` — one of the configured certificates / JWKs
//!   ([`SignatureOptions::keys`]). `RS256/384/512`, `PS256/384/512`,
//!   `ES256` and `ES384` are verified; anything else is reported as an
//!   error of the verification.
//! - `application/pkcs7-signature` (XML DigSig) and XML canonicalization are
//!   reported as not checked (information); images and PDFs are ignored, as
//!   in the Java validator.
//!
//! Like the Java validator, a verified signature says the content was not
//! changed since it was signed with the certificate's key; it does not say
//! the certificate is trustworthy. [`SignatureTrust::Listed`] adds that
//! decision: the signing certificate must be one of the configured
//! certificates or be issued (signed) by one, and valid at the signing time.

use crate::engine::{ErrorKind, Severity, ValidationError};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use ring::signature as sig;
use serde_json::Value;

/// How a verified signature's certificate is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignatureTrust {
    /// The Java validator's behaviour: the signature is checked against the
    /// certificate it carries (or names by `kid`); who issued that
    /// certificate is not judged.
    #[default]
    Any,
    /// The certificate must be one of [`SignatureOptions::keys`] or be
    /// signed by one of them, and valid at the signing time; otherwise an
    /// issue of [`SignatureOptions::untrusted_severity`].
    Listed,
}

/// Configuration of signature verification.
#[derive(Debug, Clone, Default)]
pub struct SignatureOptions {
    /// Certificates and keys: looked up by a JWS `kid`, and the trust
    /// anchors of [`SignatureTrust::Listed`].
    pub keys: Vec<TrustedKey>,
    pub trust: SignatureTrust,
    /// Severity of an untrusted certificate under [`SignatureTrust::Listed`].
    pub untrusted_severity: Severity,
}

/// A configured verification key.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    /// The key id (`kid` of a JWK; the certificate's SHA-1 thumbprint in
    /// hex and its serial number in hex for a certificate).
    pub kids: Vec<String>,
    pub key: PublicKey,
    /// The certificate's DER, when the key came from one.
    pub certificate: Option<Vec<u8>>,
}

/// A public key ring can verify with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    /// Big-endian modulus and exponent, no leading zero bytes.
    Rsa { n: Vec<u8>, e: Vec<u8> },
    /// Uncompressed SEC1 point (`04 || x || y`).
    Ec { curve: Curve, point: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    P256,
    P384,
}

impl SignatureOptions {
    /// Load certificates and keys from files: PEM (`CERTIFICATE` blocks),
    /// DER certificates, JWK or JWK Set JSON. Returns the keys and one
    /// message per file that could not be read.
    pub fn load_keys(paths: &[std::path::PathBuf]) -> (Vec<TrustedKey>, Vec<String>) {
        let mut keys = Vec::new();
        let mut problems = Vec::new();
        for path in paths {
            match std::fs::read(path) {
                Ok(bytes) => match parse_key_file(&bytes) {
                    Ok(mut found) if !found.is_empty() => keys.append(&mut found),
                    Ok(_) => problems.push(format!("{}: no certificate or key found", path.display())),
                    Err(e) => problems.push(format!("{}: {e}", path.display())),
                },
                Err(e) => problems.push(format!("{}: {e}", path.display())),
            }
        }
        (keys, problems)
    }
}

/// Parse a PEM / DER / JWK / JWKS file into keys.
pub fn parse_key_file(bytes: &[u8]) -> Result<Vec<TrustedKey>, String> {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') {
        let json: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON: {e}"))?;
        let jwks: Vec<&Value> = match json.get("keys").and_then(Value::as_array) {
            Some(keys) => keys.iter().collect(),
            None => vec![&json],
        };
        let mut out = Vec::new();
        for jwk in jwks {
            if let Some(key) = key_from_jwk(jwk) {
                out.push(key);
            }
        }
        return Ok(out);
    }
    if trimmed.contains("-----BEGIN") {
        let mut out = Vec::new();
        for der in pem_blocks(&text, "CERTIFICATE") {
            out.push(key_from_certificate(&der)?);
        }
        return Ok(out);
    }
    Ok(vec![key_from_certificate(bytes)?])
}

fn pem_blocks(text: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else {
            break;
        };
        let body: String = after[..stop].chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(der) = STANDARD.decode(body.as_bytes()) {
            out.push(der);
        }
        rest = &after[stop + end.len()..];
    }
    out
}

fn key_from_certificate(der: &[u8]) -> Result<TrustedKey, String> {
    let cert = Certificate::parse(der).ok_or("not a DER X.509 certificate")?;
    let key = cert.public_key().ok_or("unsupported public key type")?;
    let mut kids = vec![hex(&sha1_digest(der))];
    kids.push(hex(cert.serial));
    Ok(TrustedKey {
        kids,
        key,
        certificate: Some(der.to_vec()),
    })
}

fn key_from_jwk(jwk: &Value) -> Option<TrustedKey> {
    let field = |name: &str| {
        jwk.get(name)
            .and_then(Value::as_str)
            .and_then(|s| b64url_decode(s))
    };
    let key = match jwk.get("kty").and_then(Value::as_str)? {
        "RSA" => PublicKey::Rsa {
            n: strip_zeros(&field("n")?),
            e: strip_zeros(&field("e")?),
        },
        "EC" => {
            let curve = match jwk.get("crv").and_then(Value::as_str)? {
                "P-256" => Curve::P256,
                "P-384" => Curve::P384,
                _ => return None,
            };
            let mut point = vec![0x04];
            point.extend(field("x")?);
            point.extend(field("y")?);
            PublicKey::Ec { curve, point }
        }
        _ => return None,
    };
    // A JWK may carry its certificate chain.
    let certificate = jwk
        .get("x5c")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(Value::as_str)
        .and_then(|c| STANDARD.decode(c).ok());
    let mut kids: Vec<String> = jwk
        .get("kid")
        .and_then(Value::as_str)
        .map(str::to_string)
        .into_iter()
        .collect();
    if let Some(der) = &certificate {
        kids.push(hex(&sha1_digest(der)));
    }
    Some(TrustedKey {
        kids,
        key,
        certificate,
    })
}

// ------------------------------------------------------------------ issues

const WIP_NOTE: &str = "Signature Verification is a work in progress. Feedback welcome at https://chat.fhir.org/#narrow/channel/179247-Security-and-Privacy/topic/Signature/with/524324965";

struct Report<'a> {
    path: &'a str,
    out: &'a mut Vec<ValidationError>,
}

impl Report<'_> {
    fn push(&mut self, severity: Severity, message: String) {
        self.out.push(
            ValidationError::new(ErrorKind::Signature, self.path.to_string(), message)
                .with_severity(severity),
        );
    }
    fn error(&mut self, message: String) {
        self.push(Severity::Error, message);
    }
    fn warning(&mut self, message: String) {
        self.push(Severity::Warning, message);
    }
    fn info(&mut self, message: String) {
        self.push(Severity::Information, message);
    }
}

/// Verify the signature of every signed Bundle in `resource` (the root, and
/// Bundles nested in entries, contained resources or `Parameters`). Issues
/// sit on the Bundle's path.
pub fn signature_issues(resource: &Value, opts: &SignatureOptions) -> Vec<ValidationError> {
    let mut out = Vec::new();
    let root = resource
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut path = root;
    find_signed(resource, &mut path, opts, &mut out);
    out
}

fn find_signed(value: &Value, path: &mut String, opts: &SignatureOptions, out: &mut Vec<ValidationError>) {
    match value {
        Value::Object(map) => {
            if map.get("resourceType").and_then(Value::as_str) == Some("Bundle")
                && let Some(signature) = map.get("signature").filter(|s| s.is_object())
            {
                let mut report = Report { path, out };
                check_signature(value, signature, opts, &mut report);
            }
            for (key, child) in map {
                if key == "signature" || key.starts_with('_') || !(child.is_object() || child.is_array()) {
                    continue;
                }
                let len = path.len();
                path.push('.');
                path.push_str(key);
                find_signed(child, path, opts, out);
                path.truncate(len);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let len = path.len();
                path.push('.');
                path.push_str(&index.to_string());
                find_signed(item, path, opts, out);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

fn str_at<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn check_signature(bundle: &Value, signature: &Value, opts: &SignatureOptions, report: &mut Report<'_>) {
    let data = str_at(signature, "data");
    let mut format = str_at(signature, "sigFormat").filter(|s| !s.is_empty()).map(str::to_string);
    if format.is_none() {
        if data.is_some() {
            report.info("There is no value for sigFormat. In the absence of a stated signature format, the signature cannot be interpreted reliably (or validated)".into());
            if data.and_then(jose_header_of).is_some_and(|h| h.get("alg").is_some()) {
                report.info("Signature looks like application/jose, so validating accordingly".into());
                format = Some("application/jose".into());
            }
        }
    }
    let Some(format) = format else {
        return;
    };
    if format.starts_with("image/") || format.starts_with("application/pdf") {
        return;
    }
    match format.as_str() {
        "application/jose" => {
            let Some(data) = data else {
                report.info(format!(
                    "There is no actual signature data, so the {format} signature cannot be checked"
                ));
                return;
            };
            let jws = if is_base64(data) {
                match lenient_b64(data).and_then(|b| String::from_utf8(b).ok()) {
                    Some(s) => s,
                    None => {
                        report.error("The signature data is not valid base64 content, so the signature cannot be checked".into());
                        return;
                    }
                }
            } else if data.split('.').count() == 3 {
                report.error("The signature data is not valid base64 content, but it looks like native application/jose, so it will be checked anyway".into());
                data.to_string()
            } else {
                report.error("The signature data is not valid base64 content, so the signature cannot be checked".into());
                return;
            };
            report.info(WIP_NOTE.into());
            check_jose(bundle, signature, jws.trim(), opts, report);
        }
        "application/pkcs7-signature" => {
            if data.is_none() {
                report.info(format!(
                    "There is no actual signature data, so the {format} signature cannot be checked"
                ));
            } else {
                report.info(format!(
                    "The signature format '{format}' is not checked by this server (XML digital signatures are not supported)"
                ));
            }
        }
        other => report.info(format!(
            "The signature format '{other}' is not checked (known formats: application/jose and application/pkcs7-signature)"
        )),
    }
}

/// The JWS header of `data` (base64 of a compact JWS, or the JWS itself).
fn jose_header_of(data: &str) -> Option<Value> {
    let decoded = lenient_b64(data)
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| data.to_string());
    let first = decoded.split('.').next()?;
    let header = b64url_decode(first)?;
    serde_json::from_slice(&header).ok()
}

fn check_jose(bundle: &Value, signature: &Value, jws: &str, opts: &SignatureOptions, report: &mut Report<'_>) {
    let parts: Vec<&str> = jws.split('.').collect();
    let header = parts
        .first()
        .and_then(|p| b64url_decode(p))
        .ok_or_else(|| "the header is not base64url".to_string())
        .and_then(|h| serde_json::from_slice::<Value>(&h).map_err(|e| e.to_string()));
    let header = match header {
        Ok(h) if h.is_object() => h,
        Ok(_) => {
            report.error("Error parsing the digital signature header: not a JSON object".into());
            return;
        }
        Err(e) => {
            report.error(format!("Error parsing the digital signature header: {e}"));
            return;
        }
    };
    if parts.len() != 3 {
        report.error(format!(
            "Error parsing the digital signature header: a JWS compact serialization has 3 parts, found {}",
            parts.len()
        ));
        return;
    }

    // 1. signing time
    let mut sig_t = header_string(&header, "sigT");
    if let Some(t) = &sig_t
        && !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_digit())
    {
        report.warning(format!("The sigT header has the wrong format: {t}"));
        sig_t = t.parse::<i64>().ok().map(format_epoch);
    }
    if sig_t.is_none()
        && let Some(iat) = header_string(&header, "iat")
    {
        match iat.parse::<i64>() {
            Ok(secs) => sig_t = Some(format_epoch(secs)),
            Err(_) => report.error(format!(
                "The time in the signature '{iat}' is not a valid instant, so it cannot be compared with Signature.when"
            )),
        }
    }
    let when = str_at(signature, "when");
    let mut signing_instant = None;
    if sig_t.is_none() {
        report.warning("The signature does not have a signing time (sigT in the header)".into());
    }
    match (when, &sig_t) {
        (None, _) => report.error("There is no stated time in the signature".into()),
        (Some(when), Some(t)) => match parse_instant(t) {
            None => report.error(format!(
                "The time in the signature '{t}' is not a valid instant, so it cannot be compared with Signature.when"
            )),
            Some(signed) => {
                signing_instant = Some(signed);
                match parse_instant(when) {
                    None => report.error(format!(
                        "Signature.when '{when}' is not a valid instant, so it cannot be compared with the time in the signature"
                    )),
                    Some(stated) if (signed - stated).abs() > 2.0 => report.error(format!(
                        "The time in the signature {t} is not the same as the stated time {when}"
                    )),
                    Some(_) => {}
                }
            }
        },
        (Some(_), None) => {}
    }

    // 2. canonicalization
    let mut xml = false;
    let mut target_canon = None;
    if let Some(target) = str_at(signature, "targetFormat") {
        let (base, params) = target.split_once(';').unwrap_or((target, ""));
        xml = base.contains("xml");
        target_canon = params
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| k.trim() == "canonicalization")
            .map(|(_, v)| v.trim().trim_matches('"').to_string());
    }
    let canon = match (header_string(&header, "canon"), target_canon) {
        (Some(canon), target) => {
            if let Some(target) = target
                && target != canon
            {
                report.warning(format!(
                    "The canonicalization in the digital signature is {canon} which doesn't match the value {target} found in the targetFormat"
                ));
            }
            xml = !canon.contains("json");
            canon
        }
        (None, Some(target)) => target,
        (None, None) => {
            let canon = format!(
                "http://hl7.org/fhir/canonicalization/{}{}",
                if xml { "xml" } else { "json" },
                if str_at(bundle, "type") == Some("document") {
                    "#document"
                } else {
                    ""
                }
            );
            report.info(format!("No canonicalization found, so using {canon}"));
            canon
        }
    };

    // 3. purpose
    let (purpose, purpose_desc) = header_purpose(&header);
    let mut purpose_ok = false;
    let mut purpose = purpose;
    let mut purpose_desc = purpose_desc;
    for t in signature.get("type").and_then(Value::as_array).into_iter().flatten() {
        let code = str_at(t, "code").unwrap_or_default();
        let p = if is_oid(code) {
            format!("urn:oid:{code}")
        } else {
            format!("{}#{code}", str_at(t, "system").unwrap_or_default())
        };
        match &purpose {
            None => {
                purpose = Some(p);
                purpose_desc = purpose_desc.or_else(|| str_at(t, "display").map(str::to_string));
                break;
            }
            Some(stated) if *stated == p => {
                purpose_ok = true;
                purpose_desc = purpose_desc.or_else(|| str_at(t, "display").map(str::to_string));
                break;
            }
            Some(_) => {}
        }
    }

    // 4. certificate / key
    let mut kid = None;
    let mut cert_der: Option<Vec<u8>> = None;
    let mut key: Option<PublicKey> = None;
    if let Some(first) = header
        .get("x5c")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(Value::as_str)
    {
        match lenient_b64(first).and_then(|der| Certificate::parse(&der).map(|c| (c.public_key(), der.clone()))) {
            Some((Some(k), der)) => {
                key = Some(k);
                cert_der = Some(der);
            }
            Some((None, _)) => report.error(
                "Unable to parse X509 Certificate: unsupported public key type".into(),
            ),
            None => report.error("Unable to parse X509 Certificate: not a DER X.509 certificate".into()),
        }
    } else if let Some(k) = header_string(&header, "kid") {
        match opts.keys.iter().find(|t| t.kids.iter().any(|id| id.eq_ignore_ascii_case(&k))) {
            Some(found) => {
                key = Some(found.key.clone());
                cert_der = found.certificate.clone();
            }
            None => report.warning(format!(
                "Didn't find a matching certificate for the 'kid' {k} in the header, so can't verify the signature"
            )),
        }
        kid = Some(k);
    } else {
        report.warning(
            "Didn't find a provided certificate or a 'kid' in the header, so can't verify the signature".into(),
        );
    }
    let cert = cert_der.as_deref().and_then(Certificate::parse);

    // who
    let who = signature.get("who");
    match &cert {
        Some(cert) => {
            let names = cert.subject_names();
            match who.and_then(|w| w.get("identifier")).and_then(|i| str_at(i, "value")) {
                Some(value) => {
                    if !names.iter().any(|n| n == value) {
                        report.info(format!(
                            "The  Signature.who.identifier '{value}' does not match a subject value from the certificate (one of {})",
                            names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(",")
                        ));
                    }
                }
                None => report.info(format!(
                    "A certificate subject identifier should also be found in Signature.who.identifier (one of {})",
                    cert.subject_dn()
                )),
            }
        }
        None if who.is_none() => report.info(
            "Neither the digital signature nor Signature.who are provided to identify the signer".into(),
        ),
        None => report.info("The Signature.who value is not found in the digital signature".into()),
    }

    // payload
    if !parts[1].is_empty() {
        report.warning(
            "The signature has a payload, but the signature should be a detached signature with no payload".into(),
        );
    }

    let Some(key) = key else {
        return;
    };
    if xml {
        report.info(format!(
            "The signature uses the canonicalization {canon}, which this server does not check (XML canonicalization is not supported)"
        ));
        return;
    }
    let document = canon.ends_with("#document");
    let to_verify = signable_bundle(bundle, document);
    let Some(signature_bytes) = b64url_decode(parts[2]) else {
        report.error("The actual signature is not valid base64: invalid base64url".into());
        return;
    };
    let alg = header_string(&header, "alg").unwrap_or_default();
    let mut message = parts[0].as_bytes().to_vec();
    message.push(b'.');
    message.extend(URL_SAFE_NO_PAD.encode(&to_verify).as_bytes());
    let verified = match verify(&alg, &key, &message, &signature_bytes) {
        Ok(v) => v,
        Err(e) => {
            report.error(format!(
                "Error verifying the signature against the certificate: {e}"
            ));
            return;
        }
    };
    if !verified {
        match &kid {
            Some(kid) => report.error(format!(
                "The signature did not verify against the certificate using the key with id {kid}"
            )),
            None => report.error("The signature did not verify against the provided certificate".into()),
        }
        if !parts[1].is_empty() {
            match b64url_decode(parts[1]) {
                None => report.error(
                    "The payload provided could not be read - invalid Base64 content (invalid base64url)".into(),
                ),
                Some(signed) if signed != to_verify => {
                    report.error(format!(
                        "The signature cannot be valid because the content to sign ({} bytes}} is not the same as the signed content ({} bytes)",
                        group_thousands(to_verify.len()),
                        group_thousands(signed.len())
                    ));
                    match (
                        serde_json::from_slice::<Value>(&to_verify),
                        serde_json::from_slice::<Value>(&signed),
                    ) {
                        (Ok(mine), Ok(theirs)) => match json_difference("$", &mine, &theirs) {
                            None => report.info("The actual JSON structures of payload and canonicalized form do match, so the issue is in the canonicalization".into()),
                            Some(diff) => report.info(format!(
                                "The actual JSON structures of payload and canonicalized form do not match: {diff}"
                            )),
                        },
                        (_, Err(e)) => report.error(format!(
                            "The payload provided could not be read - invalid JSON Structure ({e})"
                        )),
                        (Err(e), _) => report.error(format!(
                            "The payload provided could not be read - invalid JSON Structure ({e})"
                        )),
                    }
                }
                Some(_) => {
                    if parts[1] != URL_SAFE_NO_PAD.encode(&to_verify) {
                        report.error("The signed bytes are identical, but the base64 representations differ".into());
                    }
                }
            }
        }
    } else {
        report.info("The signature verified OK".into());
    }
    let verified_suffix = |claim: &str, verified: bool| {
        if verified {
            format!("{claim} (and this has been verified by the signature)")
        } else {
            claim.to_string()
        }
    };
    if let Some(cert) = &cert {
        let by = cert.subject_dn();
        report.info(if verified {
            verified_suffix(&format!("The content was signed by {by}"), true)
        } else {
            format!("The content claims to have been signed by {by}, but this has not been verified")
        });
    }
    if let Some(t) = &sig_t {
        report.info(if verified {
            verified_suffix(&format!("The content was signed at {t}"), true)
        } else {
            format!("The content claims to have been signed at {t}, but this has not been verified")
        });
    }
    if let Some(purpose) = &purpose {
        let desc = purpose_desc.as_deref().unwrap_or("null");
        report.info(if verified && purpose_ok {
            format!("The content was signed for the purpose of {desc} ({purpose}) (and this has been verified by the signature)")
        } else {
            format!("The content claims that it was signed for the purpose of {desc} ({purpose}), but this has not been verified")
        });
    }
    if let Some(cert) = &cert {
        report.info(format!(
            "Signing Certificate Details: Subject='{}', Issuer='{}', Serial='{}'",
            cert.subject_dn(),
            cert.issuer_dn(),
            hex(strip_zeros(cert.serial).as_slice()).to_ascii_uppercase()
        ));
    }

    // Trust (HFS): only meaningful for a signature that verified.
    if verified && opts.trust == SignatureTrust::Listed {
        check_trust(cert_der.as_deref(), &key, signing_instant, opts, report);
    }
}

fn check_trust(
    cert_der: Option<&[u8]>,
    key: &PublicKey,
    signing_instant: Option<f64>,
    opts: &SignatureOptions,
    report: &mut Report<'_>,
) {
    let severity = opts.untrusted_severity;
    let cert = cert_der.and_then(Certificate::parse);
    // The signing key itself is configured.
    let listed = opts.keys.iter().any(|t| {
        t.key == *key || (cert_der.is_some() && t.certificate.as_deref() == cert_der)
    });
    let issued_by_listed = !listed
        && cert.as_ref().is_some_and(|c| {
            opts.keys.iter().any(|t| {
                let issuer_subject = t
                    .certificate
                    .as_deref()
                    .and_then(Certificate::parse)
                    .map(|i| i.subject.to_vec());
                issuer_subject.is_none_or(|s| s == c.issuer) && c.signed_by(&t.key)
            })
        });
    if !listed && !issued_by_listed {
        let who = cert
            .as_ref()
            .map(|c| format!(" (subject '{}', issuer '{}')", c.subject_dn(), c.issuer_dn()))
            .unwrap_or_default();
        report.push(
            severity,
            format!("The signing certificate is not trusted by this server: it is neither a configured certificate nor issued by one{who}"),
        );
    }
    if let (Some(cert), Some(at)) = (&cert, signing_instant)
        && let (Some(from), Some(to)) = (cert.not_before, cert.not_after)
        && (at < from || at > to)
    {
        report.push(
            severity,
            format!(
                "The signing certificate was not valid at the signing time {} (valid from {} to {})",
                format_epoch(at as i64),
                format_epoch(from as i64),
                format_epoch(to as i64)
            ),
        );
    }
}

fn header_string(header: &Value, key: &str) -> Option<String> {
    match header.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn header_purpose(header: &Value) -> (Option<String>, Option<String>) {
    let comm = header
        .get("srCms")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("commId"));
    (
        comm.and_then(|c| str_at(c, "id")).map(str::to_string),
        comm.and_then(|c| str_at(c, "desc")).map(str::to_string),
    )
}

fn is_oid(code: &str) -> bool {
    let mut parts = code.split('.');
    code.contains('.')
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn group_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The first difference between two JSON values, worded like the Java
/// validator's `CompareUtilities` ("properties differ at $.x: …").
fn json_difference(path: &str, mine: &Value, theirs: &Value) -> Option<String> {
    match (mine, theirs) {
        (Value::Object(a), Value::Object(b)) => {
            for key in b.keys() {
                if !a.contains_key(key) {
                    return Some(format!("properties differ at {path}: unexpected property {key}"));
                }
            }
            for key in a.keys() {
                if !b.contains_key(key) {
                    return Some(format!("properties differ at {path}: missing property {key}"));
                }
            }
            a.iter()
                .find_map(|(k, v)| json_difference(&format!("{path}.{k}"), v, &b[k]))
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!(
                    "array properties count differs at {path}: Expected {} but found {}",
                    a.len(),
                    b.len()
                ));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (x, y))| json_difference(&format!("{path}[{i}]"), x, y))
        }
        (a, b) if a == b => None,
        (a, b) => Some(format!("values differ at {path}: Expected {a} but found {b}")),
    }
}

// --------------------------------------------------------- canonical JSON

/// The bytes a Bundle signature signs: the Bundle in FHIR canonical JSON,
/// without `signature` (and, for `#document`, without `id` and `meta`).
pub fn signable_bundle(bundle: &Value, document: bool) -> Vec<u8> {
    let mut out = String::new();
    if let Value::Object(map) = bundle {
        let filtered: Vec<(&String, &Value)> = map
            .iter()
            .filter(|(k, _)| {
                k.as_str() != "signature" && !(document && matches!(k.as_str(), "id" | "meta"))
            })
            .collect();
        write_object(&filtered, &mut out);
    } else {
        write_canonical(bundle, &mut out);
    }
    out.into_bytes()
}

/// Canonical JSON of any value (sorted members, no whitespace).
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let members: Vec<(&String, &Value)> = map.iter().collect();
            write_object(&members, out);
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::String(s) => write_string(s, out),
        Value::Number(n) => out.push_str(&canonical_number(n)),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
    }
}

fn write_object(members: &[(&String, &Value)], out: &mut String) {
    let mut sorted = members.to_vec();
    // Java sorts Strings by UTF-16 code units.
    sorted.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
    out.push('{');
    for (i, (k, v)) in sorted.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_string(k, out);
        out.push(':');
        write_canonical(v, out);
    }
    out.push('}');
}

/// Java `Utilities.escapeJson` (with Unicode whitespace escaped).
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' ' => out.push(' '),
            c if java_whitespace(c) || (c as u32) < 32 => {
                // UTF-16 code unit, lowercase hex (whitespace is all BMP).
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn java_whitespace(c: char) -> bool {
    matches!(c as u32,
        0x20 | 0x09..=0x0D | 0x85 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F
            | 0x205F | 0x3000)
}

/// A JSON number in ECMAScript `Number.prototype.toString` form (RFC 8785),
/// as the Java validator's `JsonNumberCanonicalizer`.
fn canonical_number(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    let Some(f) = n.as_f64() else {
        return n.to_string();
    };
    es6_number(f)
}

fn es6_number(f: f64) -> String {
    if f == 0.0 {
        return "0".into();
    }
    if !f.is_finite() {
        return "null".into();
    }
    // Shortest round-trip digits and the decimal exponent.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let k = digits.len() as i32;
    let n = exp + 1; // position of the decimal point
    let mut out = String::new();
    if f < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 >= 0 { '+' } else { '-' });
        out.push_str(&(n - 1).abs().to_string());
    }
    out
}

// ------------------------------------------------------------ crypto

fn verify(alg: &str, key: &PublicKey, message: &[u8], signature: &[u8]) -> Result<bool, String> {
    match key {
        PublicKey::Rsa { n, e } => {
            let params: &sig::RsaParameters = match alg {
                "RS256" => {
                    if n.len() < 256 {
                        &sig::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY
                    } else {
                        &sig::RSA_PKCS1_2048_8192_SHA256
                    }
                }
                "RS384" => &sig::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => {
                    if n.len() < 256 {
                        &sig::RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY
                    } else {
                        &sig::RSA_PKCS1_2048_8192_SHA512
                    }
                }
                "PS256" => &sig::RSA_PSS_2048_8192_SHA256,
                "PS384" => &sig::RSA_PSS_2048_8192_SHA384,
                "PS512" => &sig::RSA_PSS_2048_8192_SHA512,
                other => {
                    return Err(format!(
                        "the algorithm '{other}' cannot be used with an RSA key (supported: RS256, RS384, RS512, PS256, PS384, PS512)"
                    ));
                }
            };
            let components = sig::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() };
            Ok(components.verify(params, message, signature).is_ok())
        }
        PublicKey::Ec { curve, point } => {
            let algorithm: &dyn sig::VerificationAlgorithm = match (alg, curve) {
                ("ES256", Curve::P256) => &sig::ECDSA_P256_SHA256_FIXED,
                ("ES384", Curve::P384) => &sig::ECDSA_P384_SHA384_FIXED,
                (other, _) => {
                    return Err(format!(
                        "the algorithm '{other}' does not match the certificate's EC key (supported: ES256 with P-256, ES384 with P-384)"
                    ));
                }
            };
            Ok(sig::UnparsedPublicKey::new(algorithm, point).verify(message, signature).is_ok())
        }
    }
}

// ------------------------------------------------------------ X.509 (DER)

/// The parts of an X.509 certificate signature checking needs.
struct Certificate<'a> {
    tbs: &'a [u8],
    serial: &'a [u8],
    issuer: &'a [u8],
    subject: &'a [u8],
    spki_alg: Vec<u8>,
    spki_params: Option<Vec<u8>>,
    spki_key: &'a [u8],
    sig_alg: Vec<u8>,
    signature: &'a [u8],
    not_before: Option<f64>,
    not_after: Option<f64>,
}

/// One DER TLV: (tag, content, whole element).
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let tag = *input.first()?;
    let first = *input.get(1)?;
    let (len, header) = if first & 0x80 == 0 {
        (first as usize, 2)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..count {
            len = (len << 8) | *input.get(2 + i)? as usize;
        }
        (len, 2 + count)
    };
    let end = header.checked_add(len)?;
    let content = input.get(header..end)?;
    Some((tag, content, &input[..end], &input[end..]))
}

fn children(mut content: &[u8]) -> Option<Vec<(u8, &[u8], &[u8])>> {
    let mut out = Vec::new();
    while !content.is_empty() {
        let (tag, inner, whole, rest) = tlv(content)?;
        out.push((tag, inner, whole));
        content = rest;
    }
    Some(out)
}

impl<'a> Certificate<'a> {
    fn parse(der: &'a [u8]) -> Option<Self> {
        let (tag, content, _, _) = tlv(der)?;
        if tag != 0x30 {
            return None;
        }
        let top = children(content)?;
        let (tbs_tag, tbs_content, tbs_whole) = *top.first()?;
        if tbs_tag != 0x30 || top.len() < 3 {
            return None;
        }
        let sig_alg = oid_of(top[1].1)?;
        let (bit_tag, bits, _) = top[2];
        if bit_tag != 0x03 || bits.is_empty() {
            return None;
        }
        let fields = children(tbs_content)?;
        let offset = usize::from(fields.first()?.0 == 0xA0);
        let serial = fields.get(offset)?.1;
        let issuer = fields.get(offset + 2)?.2;
        let validity = fields.get(offset + 3)?.1;
        let subject = fields.get(offset + 4)?.2;
        let spki = fields.get(offset + 5)?.1;
        let spki_parts = children(spki)?;
        let alg_parts = children(spki_parts.first()?.1)?;
        let spki_alg = oid_text(alg_parts.first()?.1);
        let spki_params = alg_parts
            .get(1)
            .filter(|p| p.0 == 0x06)
            .map(|p| oid_text(p.1));
        let (key_tag, key_bits, _) = *spki_parts.get(1)?;
        if key_tag != 0x03 || key_bits.is_empty() {
            return None;
        }
        let times = children(validity).unwrap_or_default();
        Some(Certificate {
            tbs: tbs_whole,
            serial,
            issuer,
            subject,
            spki_alg,
            spki_params,
            spki_key: &key_bits[1..],
            sig_alg,
            signature: &bits[1..],
            not_before: times.first().and_then(|t| asn1_time(t.0, t.1)),
            not_after: times.get(1).and_then(|t| asn1_time(t.0, t.1)),
        })
    }

    fn public_key(&self) -> Option<PublicKey> {
        match self.spki_alg.as_slice() {
            b"1.2.840.113549.1.1.1" => {
                let (_, seq, _, _) = tlv(self.spki_key)?;
                let ints = children(seq)?;
                Some(PublicKey::Rsa {
                    n: strip_zeros(ints.first()?.1),
                    e: strip_zeros(ints.get(1)?.1),
                })
            }
            b"1.2.840.10045.2.1" => {
                let curve = match self.spki_params.as_deref()? {
                    b"1.2.840.10045.3.1.7" => Curve::P256,
                    b"1.3.132.0.34" => Curve::P384,
                    _ => return None,
                };
                Some(PublicKey::Ec {
                    curve,
                    point: self.spki_key.to_vec(),
                })
            }
            _ => None,
        }
    }

    /// Whether `key` signed this certificate.
    fn signed_by(&self, key: &PublicKey) -> bool {
        match key {
            PublicKey::Rsa { n, e } => {
                let params: &sig::RsaParameters = match self.sig_alg.as_slice() {
                    b"1.2.840.113549.1.1.11" => &sig::RSA_PKCS1_2048_8192_SHA256,
                    b"1.2.840.113549.1.1.12" => &sig::RSA_PKCS1_2048_8192_SHA384,
                    b"1.2.840.113549.1.1.13" => &sig::RSA_PKCS1_2048_8192_SHA512,
                    b"1.2.840.113549.1.1.5" => &sig::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
                    _ => return false,
                };
                sig::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }
                    .verify(params, self.tbs, self.signature)
                    .is_ok()
            }
            PublicKey::Ec { curve, point } => {
                let algorithm: &dyn sig::VerificationAlgorithm = match (self.sig_alg.as_slice(), curve) {
                    (b"1.2.840.10045.4.3.2", Curve::P256) => &sig::ECDSA_P256_SHA256_ASN1,
                    (b"1.2.840.10045.4.3.3", Curve::P384) => &sig::ECDSA_P384_SHA384_ASN1,
                    (b"1.2.840.10045.4.3.3", Curve::P256) => &sig::ECDSA_P256_SHA384_ASN1,
                    (b"1.2.840.10045.4.3.2", Curve::P384) => &sig::ECDSA_P384_SHA256_ASN1,
                    _ => return false,
                };
                sig::UnparsedPublicKey::new(algorithm, point)
                    .verify(self.tbs, self.signature)
                    .is_ok()
            }
        }
    }

    fn subject_dn(&self) -> String {
        render_dn(self.subject)
    }

    fn issuer_dn(&self) -> String {
        render_dn(self.issuer)
    }

    /// The values a `Signature.who.identifier` may name: the subject DN,
    /// its common name and e-mail address.
    fn subject_names(&self) -> Vec<String> {
        let mut names = vec![self.subject_dn()];
        for (oid, value) in dn_attributes(self.subject) {
            if matches!(oid.as_str(), "2.5.4.3" | "1.2.840.113549.1.9.1" | "2.5.4.5") {
                names.push(value);
            }
        }
        names
    }
}

fn oid_of(alg_seq: &[u8]) -> Option<Vec<u8>> {
    let parts = children(alg_seq)?;
    let (tag, oid, _) = *parts.first()?;
    (tag == 0x06).then(|| oid_text(oid))
}

/// Dotted text of a DER OID (as bytes, for matching).
fn oid_text(oid: &[u8]) -> Vec<u8> {
    let mut parts: Vec<u64> = Vec::new();
    let mut value: u64 = 0;
    for (i, b) in oid.iter().enumerate() {
        value = (value << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            if parts.is_empty() && i < oid.len() {
                let first = (value / 40).min(2);
                parts.push(first);
                parts.push(value - first * 40);
            } else {
                parts.push(value);
            }
            value = 0;
        }
    }
    parts
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
        .into_bytes()
}

fn dn_attributes(name: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some((_, seq, _, _)) = tlv(name) else {
        return out;
    };
    for (_, set, _) in children(seq).unwrap_or_default() {
        for (_, atv, _) in children(set).unwrap_or_default() {
            let parts = children(atv).unwrap_or_default();
            if let (Some(oid), Some(value)) = (parts.first(), parts.get(1)) {
                let oid = String::from_utf8(oid_text(oid.1)).unwrap_or_default();
                out.push((oid, String::from_utf8_lossy(value.1).into_owned()));
            }
        }
    }
    out
}

/// RFC 2253 rendering (Java `X500Principal.getName()`): RDNs in reverse
/// order, known attribute types by name, others as `oid=#hex`.
fn render_dn(name: &[u8]) -> String {
    let Some((_, seq, _, _)) = tlv(name) else {
        return String::new();
    };
    let mut rdns = Vec::new();
    for (_, set, _) in children(seq).unwrap_or_default() {
        let mut atvs = Vec::new();
        for (_, atv, _) in children(set).unwrap_or_default() {
            let parts = children(atv).unwrap_or_default();
            let (Some(oid), Some(value)) = (parts.first(), parts.get(1)) else {
                continue;
            };
            let oid_s = String::from_utf8(oid_text(oid.1)).unwrap_or_default();
            let label = match oid_s.as_str() {
                "2.5.4.3" => Some("CN"),
                "2.5.4.6" => Some("C"),
                "2.5.4.7" => Some("L"),
                "2.5.4.8" => Some("ST"),
                "2.5.4.10" => Some("O"),
                "2.5.4.11" => Some("OU"),
                "2.5.4.9" => Some("STREET"),
                "0.9.2342.19200300.100.1.25" => Some("DC"),
                "0.9.2342.19200300.100.1.1" => Some("UID"),
                _ => None,
            };
            atvs.push(match label {
                Some(label) => format!("{label}={}", escape_dn(&String::from_utf8_lossy(value.1))),
                None => format!("{oid_s}=#{}", hex(value.2)),
            });
        }
        rdns.push(atvs.join("+"));
    }
    rdns.reverse();
    rdns.join(",")
}

fn escape_dn(value: &str) -> String {
    let mut out = String::new();
    for (i, c) in value.chars().enumerate() {
        if matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';')
            || (i == 0 && matches!(c, '#' | ' '))
        {
            out.push('\\');
        }
        out.push(c);
    }
    if out.ends_with(' ') {
        out.insert(out.len() - 1, '\\');
    }
    out
}

/// UTCTime (0x17) / GeneralizedTime (0x18) → epoch seconds.
fn asn1_time(tag: u8, content: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(content).ok()?;
    let s = s.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 => {
            let yy: i64 = s.get(0..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let num = |r: std::ops::Range<usize>| rest.get(r).and_then(|v| v.parse::<i64>().ok());
    let secs = days_from_civil(year, num(0..2)?, num(2..4)?) * 86_400
        + num(4..6)? * 3600
        + num(6..8)? * 60
        + num(8..10).unwrap_or(0);
    Some(secs as f64)
}

// ------------------------------------------------------------ time

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An RFC 3339 / FHIR instant → epoch seconds (with fraction).
fn parse_instant(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|v| v.parse::<i64>().ok());
    let (year, month, day) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hour, minute, second) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let mut rest = &s[19..];
    let mut fraction = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
        fraction = format!("0.{}", &r[..digits]).parse().ok()?;
        rest = &r[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[0..1], "+" | "-") && &o[3..4] == ":" => {
            let h: i64 = o[1..3].parse().ok()?;
            let m: i64 = o[4..6].parse().ok()?;
            let sign = if &o[0..1] == "-" { -1 } else { 1 };
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(secs as f64 + fraction)
}

/// Epoch seconds → `YYYY-MM-DDTHH:MM:SSZ` (Java `ISO_INSTANT`).
fn format_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ------------------------------------------------------------ encoding

fn is_base64(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=') || b.is_ascii_whitespace())
}

/// Decode base64 the way commons-codec does: whitespace ignored, either
/// alphabet, padding optional.
fn lenient_b64(s: &str) -> Option<Vec<u8>> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(compact.trim_end_matches('=')))
        .or_else(|_| URL_SAFE.decode(&compact))
        .or_else(|_| URL_SAFE_NO_PAD.decode(compact.trim_end_matches('=')))
        .ok()
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let trimmed = s.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD_NO_PAD.decode(trimmed))
        .ok()
}

fn strip_zeros(bytes: &[u8]) -> Vec<u8> {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len().saturating_sub(1));
    bytes[start..].to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha1_digest(bytes: &[u8]) -> Vec<u8> {
    use sha1::Digest;
    sha1::Sha1::digest(bytes).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn es6_numbers() {
        for (f, s) in [
            (1.5, "1.5"),
            (100.0, "100"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (1e21, "1e+21"),
            (123456789012.5, "123456789012.5"),
            (-2.25, "-2.25"),
            (0.1, "0.1"),
        ] {
            assert_eq!(es6_number(f), s, "{f}");
        }
    }

    #[test]
    fn canonical_form_sorts_and_escapes() {
        let v = json!({"b": [1, 2.50, "x\ny"], "a": {"d": true, "c": null}, "é": "\u{a0}"});
        assert_eq!(
            canonical_json(&v),
            "{\"a\":{\"c\":null,\"d\":true},\"b\":[1,2.5,\"x\\ny\"],\"é\":\"\\u00a0\"}"
        );
    }

    #[test]
    fn document_canonicalization_drops_id_meta_signature() {
        let b = json!({"resourceType": "Bundle", "id": "x", "meta": {"versionId": "1"},
            "type": "document", "signature": {"data": "x"}});
        assert_eq!(
            String::from_utf8(signable_bundle(&b, true)).unwrap(),
            "{\"resourceType\":\"Bundle\",\"type\":\"document\"}"
        );
        assert_eq!(
            String::from_utf8(signable_bundle(&b, false)).unwrap(),
            "{\"id\":\"x\",\"meta\":{\"versionId\":\"1\"},\"resourceType\":\"Bundle\",\"type\":\"document\"}"
        );
    }

    #[test]
    fn instants() {
        assert_eq!(parse_instant("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_instant("2020-10-23T04:54:56.048+00:00"), Some(1_603_428_896.048));
        assert_eq!(parse_instant("2026-03-26T14:02:12.000+02:00"), parse_instant("2026-03-26T12:02:12Z"));
        assert_eq!(format_epoch(1_774_526_532), "2026-03-26T12:02:12Z");
    }

    #[test]
    fn unsigned_bundles_are_not_reported() {
        let b = json!({"resourceType": "Bundle", "type": "collection"});
        assert!(signature_issues(&b, &SignatureOptions::default()).is_empty());
    }

    #[test]
    fn signatures_without_format_or_data_stay_quiet_or_informational() {
        let b = json!({"resourceType": "Bundle", "type": "document",
            "signature": {"type": [{"system": "urn:iso-astm:E1762-95:2013", "code": "1.2.840.10065.1.12.1.1"}],
            "when": "2020-01-01T00:00:00Z", "who": {"display": "x"}, "sigFormat": "application/jose"}});
        let issues = signature_issues(&b, &SignatureOptions::default());
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Information);
        let b = json!({"resourceType": "Bundle", "type": "document",
            "signature": {"sigFormat": "image/png", "data": "AAAA"}});
        assert!(signature_issues(&b, &SignatureOptions::default()).is_empty());
    }
}
