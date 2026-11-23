//! "Test this auth server" (design §11.3).
//!
//! [`preflight`] checks the stored configuration (step 0) and [`runner`]
//! drives the realm through its public URLs (steps 1–10). [`diagnose`] runs
//! both and returns one [`Report`] in the Verify-panel shape.

pub mod http;
pub mod preflight;
pub mod report;
pub mod runner;

pub use preflight::{AppProbe, EndpointProvider, EndpointView, PreflightInput};
pub use report::{Check, Report, Status, Step};
pub use runner::{Fixture, RunConfig};

/// Runs the preflight and the live steps.
pub async fn diagnose(pre: &PreflightInput<'_>, cfg: RunConfig) -> Report {
    let mut report = Report::new(&cfg.realm, &cfg.issuer);
    report.steps.push(preflight::run(pre));
    runner::run(cfg, &mut report).await;
    report.finish();
    report
}

/// Runs only the configuration preflight (instant; used by the wizard's
/// review step and the settings pages before anything is created).
pub fn preflight_only(pre: &PreflightInput<'_>) -> Report {
    let issuer = pre.realm.issuer(pre.public_base);
    let mut report = Report::new(pre.realm.name.as_str(), &issuer);
    report.steps.push(preflight::run(pre));
    report.finish();
    report
}
