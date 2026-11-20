//! P5 hook: installs the manager console (`helios_ui::manager`) over the
//! endpoint registry and store, so `/ui` becomes the overview of every
//! endpoint and `/ui/e/{endpoint}/…` its pages. Runs only with
//! `HFS_MANAGER=on`; without it the console keeps its single-endpoint mode.
//!
//! Writes from the console go through [`StoreBackend`], which reloads the
//! registry after each one (read-your-write); other instances follow through
//! LISTEN + the epoch poll. When the P4 admin API lands, a backend adapter
//! over its service can replace `StoreBackend` here (audited writes).

use helios_ui::manager::{EndpointRouting, ManagerRuntime, StoreBackend, set_manager};

use super::ManagerContext;

/// Installs the console runtime for this process.
pub fn install_ui(ctx: &ManagerContext) {
    let mut backend = StoreBackend::new(ctx.store.clone()).with_registry(ctx.registry.clone());
    if let Some(ring) = &ctx.key_ring {
        backend = backend.with_key_ring(ring.clone());
    }
    set_manager(
        ManagerRuntime::new(std::sync::Arc::new(backend), ctx.registry.clone())
            .with_routing(EndpointRouting::Dispatcher),
    );
}
