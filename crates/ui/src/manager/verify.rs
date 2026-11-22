//! Verify panels (configure-then-verify, UX audit UX-02/07/08/10): the
//! view of a [`VerifyReport`] — one checklist row per step, translated by
//! code (`verify-{code}`, `verify-{code}-fix`, `verify-step-{id}`), every
//! row anchored to the form field it checks.

use std::collections::BTreeMap;

use askama::Template;
use helios_auth::verify::{VerifyReport, VerifyStatus, VerifyStep};

use crate::I18n;

/// One "use this value" button.
pub(crate) struct SuggestionView {
    pub field_id: String,
    pub value: String,
    pub label: String,
}

/// One checklist row.
pub(crate) struct StepView {
    pub id: String,
    pub status: &'static str,
    pub status_label: String,
    pub title: String,
    pub message: String,
    pub fix: Option<String>,
    pub url: Option<String>,
    pub elapsed_ms: u64,
    pub field_id: Option<String>,
    pub suggestions: Vec<SuggestionView>,
}

/// The whole panel result.
pub(crate) struct ReportView {
    pub unavailable: Option<String>,
    pub tone: &'static str,
    pub badge: String,
    pub headline: String,
    pub checked_label: String,
    pub steps: Vec<StepView>,
}

/// The rendered fragment.
#[derive(Template)]
#[template(path = "partials/manager/verify-report.html")]
pub(crate) struct VerifyFragment {
    pub i18n: I18n,
    pub r: ReportView,
}

/// Which form control a step's `field` names, per panel.
pub(crate) type FieldMap = &'static [(&'static str, &'static str)];

/// Endpoint security form (`section-security.html`, wizard step 3).
pub(crate) const AUTH_FIELDS: FieldMap = &[
    ("mode", "f-auth_mode"),
    ("issuer", "f-auth_issuer"),
    ("jwksUrl", "f-auth_jwks_url"),
    ("audience", "f-auth_audience"),
    ("algorithms", "f-auth_algorithms"),
    ("token", "verify-auth-token"),
];

/// Endpoint terminology form (`section-terminology.html`, wizard step 4).
pub(crate) const TERMINOLOGY_FIELDS: FieldMap = &[
    ("terminologyUrl", "f-term_server"),
    ("terminologyToken", "f-term_token"),
    ("terminologyTimeout", "f-term_timeout"),
];

fn status_str(s: VerifyStatus) -> &'static str {
    match s {
        VerifyStatus::Pass => "pass",
        VerifyStatus::Warn => "warn",
        VerifyStatus::Fail => "fail",
        VerifyStatus::Skipped => "skipped",
    }
}

fn step_view(i18n: &I18n, step: &VerifyStep, fields: FieldMap, report: &VerifyReport) -> StepView {
    let status = status_str(step.status);
    let mut args: BTreeMap<String, String> = step.params.clone();
    args.entry("url".into())
        .or_insert_with(|| step.url.clone().unwrap_or_default());
    let key = format!("verify-{}", step.code);
    let message = i18n
        .t_args_opt(&key, &args)
        .unwrap_or_else(|| step.detail.clone());
    let fix = if matches!(step.status, VerifyStatus::Fail | VerifyStatus::Warn) {
        i18n.t_args_opt(&format!("{key}-fix"), &args)
    } else {
        None
    };
    let title = i18n
        .t_args_opt(&format!("verify-step-{}", step.id), &args)
        .unwrap_or_else(|| step.id.clone());
    let field_id = step.field.as_deref().and_then(|f| {
        fields
            .iter()
            .find(|(name, _)| *name == f)
            .map(|(_, id)| (*id).to_string())
    });
    // Suggestions for the field this step checks (issuer as the IdP states
    // it, the JWKS URL from discovery).
    let mut suggestions = Vec::new();
    if let Some(f) = step.field.as_deref()
        && let Some(value) = report.suggestions.get(f)
        && let Some(id) = field_id.clone()
        && matches!(step.status, VerifyStatus::Fail | VerifyStatus::Warn | VerifyStatus::Pass)
    {
        suggestions.push(SuggestionView {
            field_id: id,
            value: value.clone(),
            label: i18n.t_arg("verify-use-value", "value", value.clone()),
        });
    }
    StepView {
        id: step.id.clone(),
        status,
        status_label: i18n.t(&format!("verify-status-{status}")),
        title,
        message,
        fix,
        url: step.url.clone(),
        elapsed_ms: step.elapsed_ms,
        field_id,
        suggestions,
    }
}

/// The view of `report` for the panel whose fields are `fields`.
pub(crate) fn view(i18n: &I18n, report: &VerifyReport, fields: FieldMap) -> ReportView {
    let failures = report.failures();
    let warnings = report.warnings();
    let (tone, badge, headline) = if failures > 0 {
        (
            "fail",
            i18n.t("verify-status-fail"),
            i18n.t_arg("verify-summary-fail", "count", failures),
        )
    } else if warnings > 0 {
        (
            "warn",
            i18n.t("verify-status-warn"),
            i18n.t_arg(
                &format!("verify-summary-warn-{}", report.kind),
                "count",
                warnings,
            ),
        )
    } else {
        (
            "ok",
            i18n.t("verify-status-pass"),
            i18n.t(&format!("verify-summary-ok-{}", report.kind)),
        )
    };
    let checked = chrono::DateTime::parse_from_rfc3339(&report.checked_at)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| report.checked_at.clone());
    ReportView {
        unavailable: None,
        tone,
        badge,
        headline,
        checked_label: i18n.t_arg("verify-checked-at", "time", checked),
        steps: report
            .steps
            .iter()
            .map(|s| step_view(i18n, s, fields, report))
            .collect(),
    }
}

/// A panel that could not run at all (no backend, a refused request).
pub(crate) fn unavailable(message: String) -> ReportView {
    ReportView {
        unavailable: Some(message),
        tone: "warn",
        badge: String::new(),
        headline: String::new(),
        checked_label: String::new(),
        steps: Vec::new(),
    }
}
