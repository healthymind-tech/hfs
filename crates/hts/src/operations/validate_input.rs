//! `$validate-code` requests whose input leaves nothing — or not enough — to
//! validate (HL7 tx-ecosystem `validation/validation-missing-*`).
//!
//! The rules, shared by `CodeSystem/$validate-code` and
//! `ValueSet/$validate-code`:
//!
//! * No `code`, `coding` or `codeableConcept` at all, or a bare `code` with
//!   nothing naming its code system (`url`/`system`, or `inferSystem=true`
//!   for a value set), is a malformed request: 400, `invalid` /
//!   `invalid-data`.
//! * A `coding` / CodeableConcept coding that was provided but lacks a code
//!   or a system is a validation outcome, not a request error: a warning
//!   (`invalid` / `invalid-data`) against that Coding, and `result=false`
//!   unless another coding of the CodeableConcept validates. Only the caller
//!   knows whether a code without a system is acceptable in its context, so
//!   it is a warning.
//! * Against a value set, a CodeableConcept with no usable coding also fails
//!   as a whole (`TX_GENERAL_CC_ERROR_MESSAGE`), and a systemless coding is
//!   additionally reported as not being in the value set (information).
//!
//! `inferSystem` applies only to the bare `code` parameter, never to the
//! codings of a Coding or CodeableConcept.

use serde_json::Value;

use crate::error::HtsError;
use crate::state::AppState;
use crate::traits::{TerminologyBackend, ValueSetOperations};
use crate::types::{ValidateCodeResponse, ValidationIssue};

use super::super::params::{
    coding_entries_from_codeable_concept, find_codeable_concept_param, find_resource_param,
    find_str_param,
};
use super::{RequestPath, build_validate_response};

/// Text of the request error when there is nothing to validate. Shared with
/// `$batch-validate-code`, whose fixture pins this wording.
const NO_CODE_TO_VALIDATE_TEXT: &str = "Unable to find code to validate \
     (looked for coding | codeableConcept | code+system | code+inferSystem in parameters";

const NO_SYSTEM_TEXT: &str = "Coding has no system. A code with no system has no \
     defined meaning, and it cannot be validated. A system should be provided";
const NO_CODE_TEXT: &str =
    "Coding has a system but no code, so there is no code to validate. A code should be provided";
const NO_SYSTEM_OR_CODE_TEXT: &str = "Coding has neither a system nor a code, so it cannot be \
     validated. A system and a code should be provided";

/// Message ids of the incomplete-Coding warnings; their text belongs in the
/// response `message` although they are warnings.
pub(super) const INCOMPLETE_CODING_MESSAGE_IDS: [&str; 3] = [
    "Coding_has_no_system__cannot_validate",
    "Coding_has_no_code__cannot_validate",
    "Coding_has_no_system_or_code__cannot_validate",
];

fn non_empty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The raw `coding` parameter, even when it carries no code.
fn raw_coding(params: &[Value]) -> Option<Value> {
    ["coding", "valueCoding"].iter().find_map(|name| {
        params
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|p| p.get("valueCoding"))
            .cloned()
    })
}

/// The warning for a Coding lacking a code and/or a system, or `None` when
/// it has both.
fn incomplete_coding_issue(coding: &Value, expression: &str) -> Option<ValidationIssue> {
    let (text, message_id) = match (non_empty(coding, "system"), non_empty(coding, "code")) {
        (Some(_), Some(_)) => return None,
        (None, Some(_)) => (NO_SYSTEM_TEXT, INCOMPLETE_CODING_MESSAGE_IDS[0]),
        (Some(_), None) => (NO_CODE_TEXT, INCOMPLETE_CODING_MESSAGE_IDS[1]),
        (None, None) => (NO_SYSTEM_OR_CODE_TEXT, INCOMPLETE_CODING_MESSAGE_IDS[2]),
    };
    Some(ValidationIssue {
        severity: "warning".into(),
        fhir_code: "invalid".into(),
        tx_code: "invalid-data".into(),
        text: text.into(),
        expression: Some(expression.into()),
        location: None,
        message_id: Some(message_id.into()),
    })
}

/// `url|version` of the value set a ValueSet `$validate-code` request names
/// (stored or inline), for message texts.
async fn value_set_label<B: TerminologyBackend>(state: &AppState<B>, params: &[Value]) -> String {
    let (url, version) = match find_str_param(params, "url") {
        Some(url) => {
            let version = match find_str_param(params, "valueSetVersion") {
                Some(v) => Some(v),
                None => ValueSetOperations::search(
                    state.backend(),
                    &helios_persistence::tenant::TenantContext::system(),
                    crate::types::ResourceSearchQuery {
                        url: Some(url.clone()),
                        count: Some(1),
                        ..Default::default()
                    },
                )
                .await
                .ok()
                .and_then(|mut hits| hits.pop())
                .and_then(|vs| non_empty(&vs, "version").map(str::to_owned)),
            };
            (url, version)
        }
        None => match find_resource_param(params, "valueSet") {
            Some(vs) => (
                non_empty(&vs, "url").unwrap_or_default().to_owned(),
                non_empty(&vs, "version").map(str::to_owned),
            ),
            None => (String::new(), None),
        },
    };
    match version {
        Some(v) => format!("{url}|{v}"),
        None => url,
    }
}

fn not_in_vs_info(code: &str, vs_label: &str, expression: String) -> ValidationIssue {
    ValidationIssue {
        severity: "information".into(),
        fhir_code: "code-invalid".into(),
        tx_code: "this-code-not-in-vs".into(),
        text: format!("The provided code '#{code}' was not found in the value set '{vs_label}'"),
        expression: Some(expression),
        location: None,
        message_id: Some("None_of_the_provided_codes_are_in_the_value_set_one".into()),
    }
}

fn outcome(issues: Vec<ValidationIssue>) -> ValidateCodeResponse {
    ValidateCodeResponse {
        result: false,
        message: None,
        display: None,
        system: None,
        cs_version: None,
        inactive: None,
        issues,
        caused_by_unknown_system: None,
        concept_status: None,
        normalized_code: None,
    }
}

/// `response` with `result=false`: nothing validated, although the issues
/// are only warnings (which `build_validate_response` would read as a pass).
fn failed(mut response: Value) -> Value {
    if let Some(result) = response
        .get_mut("parameter")
        .and_then(Value::as_array_mut)
        .and_then(|ps| {
            ps.iter_mut()
                .find(|p| p.get("name").and_then(Value::as_str) == Some("result"))
        })
    {
        result["valueBoolean"] = Value::Bool(false);
    }
    response
}

/// What the regular `$validate-code` path should do with the request.
pub(super) enum InputCheck {
    /// The input is complete: validate as usual.
    Proceed,
    /// The request is answered already (an incomplete Coding).
    Done(Value),
    /// A CodeableConcept with some incomplete codings next to complete ones:
    /// validate as usual, then add these issues (see [`merge_issues`]).
    ProceedThenAdd(Vec<ValidationIssue>),
}

/// Check the input of a `$validate-code` request. `value_set` is true for
/// `ValueSet/$validate-code`.
pub(super) async fn check_validate_input<B: TerminologyBackend>(
    state: &AppState<B>,
    params: &[Value],
    value_set: bool,
) -> Result<InputCheck, HtsError> {
    if let Some(cc) = find_codeable_concept_param(params) {
        let codings = coding_entries_from_codeable_concept(&cc).unwrap_or_default();
        let mut issues = Vec::new();
        let mut complete = 0usize;
        let mut vs_label: Option<String> = None;
        for (i, coding) in codings.iter().enumerate() {
            let at = format!("CodeableConcept.coding[{i}]");
            match incomplete_coding_issue(coding, &at) {
                None => complete += 1,
                Some(warning) => {
                    // A systemless code is also, as far as anyone can tell,
                    // not in the value set.
                    if value_set && non_empty(coding, "system").is_none() {
                        if let Some(code) = non_empty(coding, "code") {
                            if vs_label.is_none() {
                                vs_label = Some(value_set_label(state, params).await);
                            }
                            issues.push(not_in_vs_info(
                                code,
                                vs_label.as_deref().unwrap_or_default(),
                                format!("{at}.code"),
                            ));
                        }
                    }
                    issues.push(warning);
                }
            }
        }
        if complete > 0 {
            return Ok(if issues.is_empty() {
                InputCheck::Proceed
            } else {
                InputCheck::ProceedThenAdd(issues)
            });
        }
        if value_set {
            let label = match vs_label {
                Some(l) => l,
                None => value_set_label(state, params).await,
            };
            issues.insert(
                0,
                ValidationIssue {
                    severity: "error".into(),
                    fhir_code: "code-invalid".into(),
                    tx_code: "not-in-vs".into(),
                    text: format!("No valid coding was found for the value set '{label}'"),
                    expression: None,
                    location: None,
                    message_id: Some("TX_GENERAL_CC_ERROR_MESSAGE".into()),
                },
            );
        }
        return Ok(InputCheck::Done(failed(build_validate_response(
            outcome(issues),
            None,
            None,
            None,
            Some(&cc),
            None,
            RequestPath::CodeableConcept,
        ))));
    }

    if let Some(coding) = raw_coding(params) {
        let system = non_empty(&coding, "system");
        let code = non_empty(&coding, "code");
        // A systemless code against a value set has its own handling (it is
        // also reported as not in the value set); everything else that lacks
        // a code or a system — and a systemless code with no CodeSystem
        // named at all — is answered here.
        let cs_named =
            find_str_param(params, "url").is_some() || find_str_param(params, "system").is_some();
        let answer_here = match (system, code) {
            (Some(_), Some(_)) => false,
            (None, Some(_)) => !value_set && !cs_named,
            _ => true,
        };
        if answer_here {
            if let Some(warning) = incomplete_coding_issue(&coding, "Coding") {
                return Ok(InputCheck::Done(failed(build_validate_response(
                    outcome(vec![warning]),
                    code,
                    system,
                    None,
                    None,
                    None,
                    RequestPath::Coding,
                ))));
            }
        }
        return Ok(InputCheck::Proceed);
    }

    match find_str_param(params, "code") {
        None => Err(HtsError::InvalidRequest(NO_CODE_TO_VALIDATE_TEXT.into())),
        Some(code) => {
            let system_named = find_str_param(params, "system").is_some()
                || (!value_set && find_str_param(params, "url").is_some());
            let infer = value_set
                && params.iter().any(|p| {
                    p.get("name").and_then(Value::as_str) == Some("inferSystem")
                        && (p.get("valueBoolean") == Some(&Value::Bool(true))
                            || p.get("valueString").and_then(Value::as_str) == Some("true"))
                });
            if system_named || infer {
                Ok(InputCheck::Proceed)
            } else if value_set {
                Err(HtsError::InvalidRequest(format!(
                    "Unable to validate the code '{code}': no system was provided, and \
                     inferSystem is not true"
                )))
            } else {
                Err(HtsError::InvalidRequest(format!(
                    "Unable to validate the code '{code}': no CodeSystem url was provided"
                )))
            }
        }
    }
}

/// Add the incomplete-coding `issues` of a CodeableConcept to a regular
/// `$validate-code` response: they go into its OperationOutcome, the warning
/// texts into `message`, and a response with no error stays `result=true`.
pub(super) fn merge_issues(response: &mut Value, issues: Vec<ValidationIssue>) {
    let Some(parameter) = response.get_mut("parameter").and_then(Value::as_array_mut) else {
        return;
    };
    let rendered: Vec<Value> = issues.iter().map(super::render_issue).collect();
    match parameter
        .iter_mut()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("issues"))
        .and_then(|p| p.pointer_mut("/resource/issue"))
        .and_then(Value::as_array_mut)
    {
        Some(existing) => existing.extend(rendered),
        None => parameter.push(serde_json::json!({
            "name": "issues",
            "resource": {"resourceType": "OperationOutcome", "issue": rendered}
        })),
    }
    let mut texts: Vec<String> = parameter
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("message"))
        .and_then(|p| p.get("valueString").and_then(Value::as_str))
        .map(|m| m.split("; ").map(str::to_owned).collect())
        .unwrap_or_default();
    texts.extend(
        issues
            .iter()
            .filter(|i| i.severity != "information")
            .map(|i| i.text.clone()),
    );
    texts.sort();
    texts.dedup();
    let message = texts.join("; ");
    match parameter
        .iter_mut()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("message"))
    {
        Some(p) => p["valueString"] = Value::String(message),
        None => parameter.push(serde_json::json!({"name": "message", "valueString": message})),
    }
    // Keep the Parameters in name order, as every other response is.
    parameter.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .cmp(&b.get("name").and_then(Value::as_str))
    });
}
