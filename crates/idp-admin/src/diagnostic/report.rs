//! The diagnostic report (design §11.3), in the shape of the console's
//! shared Verify panel (UX audit UX-02): an ordered checklist of steps, each
//! with checks that are `pass` / `warn` / `fail` / `skipped`, a stable
//! `code` (translated by the console as Fluent `idpdiag-<code>` and
//! `idpdiag-<code>-fix` with `params`), the `field` to jump to ("Go to
//! field"), the URL contacted, the spec reference and timing. The English
//! `message` / `fix` are filled too, so the JSON report (CI, downloads) is
//! readable without the catalogs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Outcome of one check or step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Skipped,
    Warn,
    Fail,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Skipped => "skipped",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

/// One check inside a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    /// Stable id, unique within the report (`discovery.issuer`).
    pub id: String,
    pub status: Status,
    /// Message code; Fluent keys `idpdiag-<code>` / `idpdiag-<code>-fix`.
    pub code: String,
    /// Placeables of the message (`expected`, `actual`, `url`, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Console field the fix applies to (wizard / settings anchor), e.g.
    /// `client.app.redirectUris`, `keys`, `endpoints.clinic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// URL contacted, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Spec section the check enforces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    /// English message (fallback; the console translates `code`).
    pub message: String,
    /// English remediation; empty for passing checks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fix: String,
}

impl Check {
    pub fn new(id: &str, status: Status, code: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status,
            code: code.to_string(),
            params: BTreeMap::new(),
            field: None,
            url: None,
            spec: None,
            message: message.into(),
            fix: String::new(),
        }
    }
    pub fn pass(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self::new(id, Status::Pass, code, message)
    }
    pub fn fail(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self::new(id, Status::Fail, code, message)
    }
    pub fn warn(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self::new(id, Status::Warn, code, message)
    }
    pub fn skipped(id: &str, code: &str, message: impl Into<String>) -> Self {
        Self::new(id, Status::Skipped, code, message)
    }
    pub fn param(mut self, k: &str, v: impl Into<String>) -> Self {
        self.params.insert(k.to_string(), v.into());
        self
    }
    pub fn field(mut self, f: impl Into<String>) -> Self {
        self.field = Some(f.into());
        self
    }
    pub fn url(mut self, u: impl Into<String>) -> Self {
        self.url = Some(u.into());
        self
    }
    pub fn spec(mut self, s: impl Into<String>) -> Self {
        self.spec = Some(s.into());
        self
    }
    pub fn fix(mut self, f: impl Into<String>) -> Self {
        self.fix = f.into();
        self
    }
}

/// One numbered step of the checklist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// 0 = configuration preflight, 1..=10 = live steps of design §11.3.
    pub n: u8,
    /// Stable id (`discovery`, `jwks`, `authorization-code`, …); Fluent
    /// title key `idpdiag-step-<id>`.
    pub id: String,
    pub title: String,
    pub status: Status,
    pub duration_ms: u64,
    pub checks: Vec<Check>,
}

impl Step {
    pub fn new(n: u8, id: &str, title: &str) -> Self {
        Self {
            n,
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Pass,
            duration_ms: 0,
            checks: Vec::new(),
        }
    }

    pub fn push(&mut self, c: Check) {
        self.checks.push(c);
    }

    /// Worst check status (`skipped` only when every check was skipped).
    pub fn finish(&mut self) {
        let worst = self.checks.iter().map(|c| c.status).max();
        self.status = match worst {
            None => Status::Skipped,
            Some(Status::Skipped) => Status::Skipped,
            Some(s) => {
                if s == Status::Pass
                    && self.checks.iter().all(|c| c.status == Status::Skipped)
                {
                    Status::Skipped
                } else {
                    s
                }
            }
        };
    }

    /// The whole step skipped because an earlier step it depends on failed.
    pub fn skipped_because(n: u8, id: &str, title: &str, depends_on: &str) -> Self {
        let mut s = Self::new(n, id, title);
        s.push(
            Check::skipped(&format!("{id}.dependency"), "skipped-dependency", format!(
                "Skipped: it depends on step \"{depends_on}\", which failed."
            ))
            .param("step", depends_on),
        );
        s.finish();
        s
    }
}

/// A full diagnostic run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub realm: String,
    pub issuer: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: Status,
    pub steps: Vec<Step>,
}

impl Report {
    pub fn new(realm: &str, issuer: &str) -> Self {
        let now = Utc::now();
        Self {
            realm: realm.to_string(),
            issuer: issuer.to_string(),
            started_at: now,
            finished_at: now,
            status: Status::Pass,
            steps: Vec::new(),
        }
    }

    pub fn finish(&mut self) {
        self.finished_at = Utc::now();
        self.status = self
            .steps
            .iter()
            .map(|s| s.status)
            .filter(|s| *s != Status::Skipped)
            .max()
            .unwrap_or(Status::Skipped);
    }

    /// Every failing check, in order (the first is the one to fix first;
    /// the others may be consequences).
    pub fn problems(&self) -> Vec<&Check> {
        self.steps
            .iter()
            .flat_map(|s| s.checks.iter())
            .filter(|c| c.status == Status::Fail)
            .collect()
    }

    pub fn warnings(&self) -> Vec<&Check> {
        self.steps
            .iter()
            .flat_map(|s| s.checks.iter())
            .filter(|c| c.status == Status::Warn)
            .collect()
    }

    pub fn check(&self, id: &str) -> Option<&Check> {
        self.steps
            .iter()
            .flat_map(|s| s.checks.iter())
            .find(|c| c.id == id)
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_status_is_worst_check() {
        let mut s = Step::new(1, "discovery", "Discovery");
        s.push(Check::pass("a", "ok", "ok"));
        s.push(Check::warn("b", "w", "w"));
        s.finish();
        assert_eq!(s.status, Status::Warn);
        s.push(Check::fail("c", "f", "f"));
        s.finish();
        assert_eq!(s.status, Status::Fail);
        let s = Step::skipped_because(3, "authorization-code", "Auth code", "discovery");
        assert_eq!(s.status, Status::Skipped);
    }

    #[test]
    fn report_ignores_skipped_and_wire_shape() {
        let mut r = Report::new("p", "https://h/idp/p");
        let mut a = Step::new(1, "discovery", "Discovery");
        a.push(Check::pass("discovery.issuer", "issuer-ok", "ok"));
        a.finish();
        r.steps.push(a);
        r.steps
            .push(Step::skipped_because(2, "jwks", "JWKS", "discovery"));
        r.finish();
        assert_eq!(r.status, Status::Pass);
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["steps"][0]["checks"][0]["status"], "pass");
        assert_eq!(j["steps"][1]["status"], "skipped");
        assert!(j["steps"][0]["checks"][0].get("fix").is_none());
    }
}
