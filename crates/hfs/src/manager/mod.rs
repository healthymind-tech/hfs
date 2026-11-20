//! FHIR server manager wiring, one file per package
//! (`docs/design/fhir-server-manager.md` §15). P1 owns this module file and
//! its `ManagerContext`; packages add their `install_*` modules here.

pub mod sync;
