//! Free-space checks before writes that grow the database or the staging
//! area.

use std::path::Path;

/// Bytes available to an unprivileged writer on the filesystem holding
/// `path` (walking up to the nearest existing ancestor). `None` when it
/// cannot be determined (non-Unix, or `statvfs` failed) — callers then skip
/// the check rather than refuse the operation.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    free_bytes_impl(p)
}

#[cfg(unix)]
fn free_bytes_impl(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized,
    // writable statvfs struct for the duration of the call.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(not(unix))]
fn free_bytes_impl(_path: &Path) -> Option<u64> {
    None
}

/// Outcome of a space check, serialised into job errors and the detect
/// response so the operator sees the numbers behind a refusal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpaceCheck {
    pub path: String,
    pub free_bytes: Option<u64>,
    pub required_bytes: u64,
    pub ok: bool,
}

/// Is there room for `required` bytes plus `headroom` at `path`?
pub fn check(path: &Path, required: u64, headroom: u64) -> SpaceCheck {
    let free = free_bytes(path);
    let need = required.saturating_add(headroom);
    SpaceCheck {
        path: path.display().to_string(),
        free_bytes: free,
        required_bytes: need,
        ok: free.is_none_or(|f| f >= need),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_temp_dir_is_known_on_unix() {
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("not/yet/created");
        let f = free_bytes(&missing);
        if cfg!(unix) {
            assert!(f.unwrap() > 0);
        }
        assert!(check(&missing, 1, 0).ok);
        assert!(!check(&missing, u64::MAX / 2, 0).ok || f.is_none());
    }
}
