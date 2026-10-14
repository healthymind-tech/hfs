//! Chunked, resumable uploads into a staging directory, and the read-only
//! server import directory.
//!
//! # Upload protocol
//!
//! 1. `POST /admin/uploads {filename, size, client_key?}` creates a session and
//!    returns `{id, chunk_size, chunks, received[]}`. When an *incomplete*
//!    session with the same `client_key` (the browser derives it from file
//!    name + size + mtime) already exists, that session is returned instead —
//!    this is how a reloaded or crashed browser resumes.
//! 2. `PUT /admin/uploads/{id}/chunks/{index}` with the raw bytes of chunk
//!    `index` (`chunk_size` bytes, the last one shorter). An optional
//!    `X-Chunk-Sha256` header is verified. Chunks are written in place at
//!    `index * chunk_size` into a pre-sized `data.part`, so they may arrive in
//!    any order, be retried, or be sent in parallel. Re-sending a received
//!    chunk is harmless.
//! 3. `POST /admin/uploads/{id}/complete` checks every chunk arrived, computes
//!    the file's SHA-256, renames it to its (sanitised) original name and runs
//!    format detection.
//!
//! The session state (`meta.json`) is rewritten atomically after every chunk,
//! so a server restart loses nothing either.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use super::AdminError;
use super::formats::{self, Detection};

/// Persisted state of one upload session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMeta {
    pub id: String,
    pub filename: String,
    pub size: u64,
    pub chunk_size: u64,
    pub chunks: u64,
    /// One flag per chunk.
    pub received: Vec<bool>,
    pub client_key: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed: bool,
    pub sha256: Option<String>,
    pub detection: Option<Detection>,
    pub uploaded_by: Option<String>,
}

impl UploadMeta {
    /// Bytes received so far.
    pub fn received_bytes(&self) -> u64 {
        self.received
            .iter()
            .enumerate()
            .filter(|(_, r)| **r)
            .map(|(i, _)| self.chunk_len(i as u64))
            .sum()
    }

    /// Expected length of chunk `index`.
    pub fn chunk_len(&self, index: u64) -> u64 {
        if index + 1 < self.chunks {
            self.chunk_size
        } else {
            self.size - self.chunk_size * (self.chunks.saturating_sub(1))
        }
    }

    /// Indexes still missing.
    pub fn missing(&self) -> Vec<u64> {
        self.received
            .iter()
            .enumerate()
            .filter(|(_, r)| !**r)
            .map(|(i, _)| i as u64)
            .collect()
    }
}

/// Upload session store rooted at `<admin data dir>/uploads`.
pub struct UploadStore {
    dir: PathBuf,
    pub chunk_size: u64,
    pub max_size: u64,
    sessions: std::sync::Mutex<HashMap<String, Arc<Mutex<UploadMeta>>>>,
}

/// Keep `[A-Za-z0-9._-]`, map the rest to `_`, never empty, never a dotfile.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while s.starts_with('.') {
        s.remove(0);
    }
    if s.len() > 160 {
        // Keep the extension(s) — format detection relies on them.
        let cut = s.len() - 160;
        s.drain(..cut);
    }
    if s.is_empty() {
        s = "upload.bin".into();
    }
    s
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn io(e: impl std::fmt::Display) -> AdminError {
    AdminError::Internal(e.to_string())
}

impl UploadStore {
    pub fn new(dir: PathBuf, chunk_size: u64, max_size: u64) -> Self {
        Self {
            dir,
            chunk_size: chunk_size.max(64 * 1024),
            max_size,
            sessions: Default::default(),
        }
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn valid_id(id: &str) -> bool {
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    }

    async fn session(&self, id: &str) -> Result<Arc<Mutex<UploadMeta>>, AdminError> {
        if !Self::valid_id(id) {
            return Err(AdminError::NotFound(format!("upload {id}")));
        }
        if let Some(s) = self.sessions.lock().unwrap().get(id).cloned() {
            return Ok(s);
        }
        let meta_path = self.session_dir(id).join("meta.json");
        let bytes = tokio::fs::read(&meta_path)
            .await
            .map_err(|_| AdminError::NotFound(format!("upload {id}")))?;
        let meta: UploadMeta = serde_json::from_slice(&bytes).map_err(io)?;
        let arc = Arc::new(Mutex::new(meta));
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| arc.clone());
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or(arc))
    }

    /// All sessions on disk, newest first.
    pub async fn list(&self) -> Vec<UploadMeta> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(&self.dir).await else {
            return out;
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let id = e.file_name().to_string_lossy().into_owned();
            if let Ok(s) = self.session(&id).await {
                out.push(s.lock().await.clone());
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Create a session, or return the incomplete one with the same
    /// `client_key` (resume).
    pub async fn create(
        &self,
        filename: &str,
        size: u64,
        client_key: Option<String>,
        uploaded_by: Option<String>,
    ) -> Result<UploadMeta, AdminError> {
        if size == 0 {
            return Err(AdminError::BadRequest("size must be > 0".into()));
        }
        if size > self.max_size {
            return Err(AdminError::TooLarge(format!(
                "{size} bytes exceeds HTS_MAX_UPLOAD_SIZE ({} bytes)",
                self.max_size
            )));
        }
        let filename = sanitize_filename(filename);
        if let Some(key) = client_key.as_deref() {
            for m in self.list().await {
                if !m.completed
                    && m.client_key.as_deref() == Some(key)
                    && m.size == size
                    && m.filename == filename
                {
                    return Ok(m);
                }
            }
        }
        let space = super::disk::check(&self.dir, size, 256 * 1024 * 1024);
        if !space.ok {
            return Err(AdminError::InsufficientStorage(
                serde_json::to_value(&space).unwrap(),
            ));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = self.session_dir(&id);
        tokio::fs::create_dir_all(&dir).await.map_err(io)?;
        let part = dir.join("data.part");
        let f = std::fs::File::create(&part).map_err(io)?;
        f.set_len(size).map_err(io)?;
        let chunks = size.div_ceil(self.chunk_size);
        let now = chrono::Utc::now().to_rfc3339();
        let meta = UploadMeta {
            id: id.clone(),
            filename,
            size,
            chunk_size: self.chunk_size,
            chunks,
            received: vec![false; chunks as usize],
            client_key,
            created_at: now.clone(),
            updated_at: now,
            completed: false,
            sha256: None,
            detection: None,
            uploaded_by,
        };
        write_json_atomic(&dir.join("meta.json"), &meta).map_err(io)?;
        self.sessions
            .lock()
            .unwrap()
            .insert(id, Arc::new(Mutex::new(meta.clone())));
        Ok(meta)
    }

    pub async fn status(&self, id: &str) -> Result<UploadMeta, AdminError> {
        Ok(self.session(id).await?.lock().await.clone())
    }

    /// Store chunk `index`.
    pub async fn put_chunk(
        &self,
        id: &str,
        index: u64,
        data: bytes::Bytes,
        sha256_hex: Option<&str>,
    ) -> Result<UploadMeta, AdminError> {
        let session = self.session(id).await?;
        let mut meta = session.lock().await;
        if meta.completed {
            return Err(AdminError::Conflict("upload already completed".into()));
        }
        if index >= meta.chunks {
            return Err(AdminError::BadRequest(format!(
                "chunk {index} out of range (0..{})",
                meta.chunks
            )));
        }
        let expected = meta.chunk_len(index);
        if data.len() as u64 != expected {
            return Err(AdminError::BadRequest(format!(
                "chunk {index} has {} bytes, expected {expected}",
                data.len()
            )));
        }
        if let Some(want) = sha256_hex {
            let got = hex_digest(&data);
            if !got.eq_ignore_ascii_case(want.trim()) {
                return Err(AdminError::BadRequest(format!(
                    "chunk {index} checksum mismatch (got {got})"
                )));
            }
        }
        let part = self.session_dir(id).join("data.part");
        let offset = index * meta.chunk_size;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().write(true).open(part)?;
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(&data)?;
            f.sync_data()
        })
        .await
        .map_err(io)?
        .map_err(io)?;
        meta.received[index as usize] = true;
        meta.updated_at = chrono::Utc::now().to_rfc3339();
        write_json_atomic(&self.session_dir(id).join("meta.json"), &*meta).map_err(io)?;
        Ok(meta.clone())
    }

    /// Finish: verify, hash, rename, detect.
    pub async fn complete(&self, id: &str) -> Result<UploadMeta, AdminError> {
        let session = self.session(id).await?;
        let mut meta = session.lock().await;
        if meta.completed {
            return Ok(meta.clone());
        }
        let missing = meta.missing();
        if !missing.is_empty() {
            return Err(AdminError::Conflict(format!(
                "{} chunk(s) missing, first: {}",
                missing.len(),
                missing[0]
            )));
        }
        let dir = self.session_dir(id);
        let part = dir.join("data.part");
        let final_path = dir.join(&meta.filename);
        let (sha, detection) =
            tokio::task::spawn_blocking(move || -> std::io::Result<(String, Detection)> {
                let mut f = std::fs::File::open(&part)?;
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&buf[..n]);
                }
                std::fs::rename(&part, &final_path)?;
                let detection = formats::detect(&final_path);
                Ok((hex(&h.finalize()), detection))
            })
            .await
            .map_err(io)?
            .map_err(io)?;
        meta.sha256 = Some(sha);
        meta.detection = Some(detection);
        meta.completed = true;
        meta.updated_at = chrono::Utc::now().to_rfc3339();
        write_json_atomic(&dir.join("meta.json"), &*meta).map_err(io)?;
        Ok(meta.clone())
    }

    /// Path of a completed upload's file.
    pub async fn file_path(&self, id: &str) -> Result<(PathBuf, UploadMeta), AdminError> {
        let meta = self.status(id).await?;
        if !meta.completed {
            return Err(AdminError::Conflict(format!("upload {id} is not complete")));
        }
        Ok((self.session_dir(id).join(&meta.filename), meta))
    }

    pub async fn delete(&self, id: &str) -> Result<(), AdminError> {
        let session = self.session(id).await?;
        let _guard = session.lock().await;
        tokio::fs::remove_dir_all(self.session_dir(id))
            .await
            .map_err(io)?;
        self.sessions.lock().unwrap().remove(id);
        Ok(())
    }

    /// Remove incomplete sessions untouched for `max_age`.
    pub async fn purge_stale(&self, max_age: chrono::Duration) -> usize {
        let cutoff = chrono::Utc::now() - max_age;
        let mut n = 0;
        for m in self.list().await {
            let stale = chrono::DateTime::parse_from_rfc3339(&m.updated_at)
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if !m.completed && stale && self.delete(&m.id).await.is_ok() {
                n += 1;
            }
        }
        n
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// SHA-256 of `data` as lowercase hex.
pub fn hex_digest(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

// ── Server import directory ──────────────────────────────────────────────────

/// A file under `HTS_IMPORT_DIR`.
#[derive(Debug, Clone, Serialize)]
pub struct ImportDirEntry {
    /// Path relative to the import directory (`/`-separated).
    pub path: String,
    pub size: u64,
    pub modified: Option<String>,
    pub is_dir: bool,
    /// Cheap name-based guess; full detection runs on `GET …/detect`.
    pub format_hint: Option<String>,
}

/// List regular files (and RxNorm-style directories) up to two levels deep.
pub fn list_import_dir(root: &Path) -> Result<Vec<ImportDirEntry>, AdminError> {
    let mut out = Vec::new();
    walk(root, root, 0, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<ImportDirEntry>,
) -> Result<(), AdminError> {
    let rd = std::fs::read_dir(dir)
        .map_err(|e| AdminError::NotFound(format!("import directory {}: {e}", dir.display())))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let modified = meta
            .modified()
            .ok()
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
        if meta.is_dir() {
            let rrf = std::fs::read_dir(&path)
                .map(|rd| {
                    rd.flatten().any(|f| {
                        f.file_name()
                            .to_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case("RXNCONSO.RRF"))
                    })
                })
                .unwrap_or(false);
            if rrf {
                out.push(ImportDirEntry {
                    path: rel,
                    size: 0,
                    modified,
                    is_dir: true,
                    format_hint: Some("rxnorm".into()),
                });
            } else if depth < 1 {
                walk(root, &path, depth + 1, out)?;
            }
        } else if meta.is_file() {
            let hint = crate::config::detect_format(Path::new(&name))
                .filter(|_| !name.to_ascii_lowercase().ends_with(".zip"))
                .map(|f| f.to_string());
            out.push(ImportDirEntry {
                path: rel,
                size: meta.len(),
                modified,
                is_dir: false,
                format_hint: hint,
            });
        }
    }
    Ok(())
}

/// Resolve `rel` inside `root`, refusing anything that escapes it (`..`,
/// absolute paths, symlinks pointing outside).
pub fn resolve_in(root: &Path, rel: &str) -> Result<PathBuf, AdminError> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.split('/').any(|seg| seg == "..") {
        return Err(AdminError::BadRequest("invalid path".into()));
    }
    let root = root
        .canonicalize()
        .map_err(|e| AdminError::NotFound(format!("import directory: {e}")))?;
    let p = root
        .join(rel)
        .canonicalize()
        .map_err(|_| AdminError::NotFound(format!("{rel} not found in import directory")))?;
    if !p.starts_with(&root) {
        return Err(AdminError::BadRequest(
            "path escapes the import directory".into(),
        ));
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn chunks_assemble_out_of_order_and_resume_by_client_key() {
        let d = tempfile::tempdir().unwrap();
        let store = UploadStore::new(d.path().join("uploads"), 64 * 1024, 1 << 30);
        let data: Vec<u8> = (0..(64 * 1024 * 3 + 100))
            .map(|i| (i % 251) as u8)
            .collect();
        let m = store
            .create(
                "../evil/My File.zip",
                data.len() as u64,
                Some("k1".into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(m.filename, "My_File.zip");
        assert_eq!(m.chunks, 4);
        let chunk = |i: u64| {
            let s = (i * m.chunk_size) as usize;
            let e = ((i + 1) * m.chunk_size).min(data.len() as u64) as usize;
            bytes::Bytes::copy_from_slice(&data[s..e])
        };
        // Last chunk first, with a checksum.
        let c3 = chunk(3);
        let sum = hex_digest(&c3);
        store.put_chunk(&m.id, 3, c3, Some(&sum)).await.unwrap();
        store.put_chunk(&m.id, 0, chunk(0), None).await.unwrap();
        // Wrong checksum and wrong length are rejected.
        assert!(
            store
                .put_chunk(&m.id, 1, chunk(1), Some("00"))
                .await
                .is_err()
        );
        assert!(store.put_chunk(&m.id, 1, chunk(3), None).await.is_err());
        // Completing early names the gap.
        assert!(matches!(
            store.complete(&m.id).await,
            Err(AdminError::Conflict(_))
        ));

        // "Browser restart": a fresh store over the same directory, same key.
        let store2 = UploadStore::new(d.path().join("uploads"), 64 * 1024, 1 << 30);
        let resumed = store2
            .create("My File.zip", data.len() as u64, Some("k1".into()), None)
            .await
            .unwrap();
        assert_eq!(resumed.id, m.id);
        assert_eq!(resumed.missing(), vec![1, 2]);
        for i in resumed.missing() {
            store2.put_chunk(&m.id, i, chunk(i), None).await.unwrap();
        }
        let done = store2.complete(&m.id).await.unwrap();
        assert!(done.completed);
        assert_eq!(done.sha256.as_deref(), Some(hex_digest(&data).as_str()));
        let (path, _) = store2.file_path(&m.id).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), data);
        // A completed session is not resumed: same key creates a new one.
        let fresh = store2
            .create("My File.zip", data.len() as u64, Some("k1".into()), None)
            .await
            .unwrap();
        assert_ne!(fresh.id, m.id);
        store2.delete(&fresh.id).await.unwrap();
        assert!(store2.status(&fresh.id).await.is_err());
    }

    #[tokio::test]
    async fn rejects_oversize_and_bad_ids() {
        let d = tempfile::tempdir().unwrap();
        let store = UploadStore::new(d.path().to_path_buf(), 1 << 20, 1000);
        assert!(matches!(
            store.create("a.zip", 1001, None, None).await,
            Err(AdminError::TooLarge(_))
        ));
        assert!(store.status("../../etc").await.is_err());
    }

    #[test]
    fn import_dir_resolution_is_confined() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/a.json"), b"{}").unwrap();
        std::fs::write(d.path().join("b.tgz"), b"x").unwrap();
        let list = list_import_dir(d.path()).unwrap();
        let paths: Vec<&str> = list.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["b.tgz", "sub/a.json"]);
        assert_eq!(list[0].format_hint.as_deref(), Some("hl7-npm"));
        assert!(resolve_in(d.path(), "sub/a.json").is_ok());
        assert!(resolve_in(d.path(), "../etc/passwd").is_err());
        assert!(resolve_in(d.path(), "missing.zip").is_err());
    }
}
