use std::collections::BTreeMap;
use std::io::Write;
use std::path::Component;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tempfile::NamedTempFile;
use tracing::warn;

/// Files smaller than this are kept in memory; larger files are spilled to disk.
const ARTIFACT_MEMORY_THRESHOLD: u64 = 1024 * 1024; // 1 MB

/// Maximum total bytes accepted across all uploads for a single job.
pub const ARTIFACT_JOB_LIMIT: u64 = 1024 * 1024 * 1024; // 1 GB

/// Sanitize an artifact upload path to a safe relative path.
///
/// - Rejects absolute paths, `..` components, and empty paths.
/// - Strips redundant `.` components.
/// - Returns the normalized path joined with `/`, or `None` if the path is invalid.
pub fn sanitize_artifact_path(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let mut components = Vec::new();
    for component in p.components() {
        match component {
            Component::Normal(name) => components.push(name.to_string_lossy().into_owned()),
            Component::CurDir => {} // skip redundant .
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(components.join("/"))
}

/// Errors returned by [`UploadStore::upload_file`].
pub enum UploadError {
    /// No job is registered under the given key.
    UnknownKey,
    /// The supplied path failed sanitization.
    InvalidPath,
    /// Accepting this upload would exceed the per-job byte limit.
    LimitExceeded,
    /// An I/O error occurred while spilling the artifact to disk.
    Io(std::io::Error),
}

// ---------------------------------------------------------------------------
// ArtifactFile — memory or disk backed
// ---------------------------------------------------------------------------

enum ArtifactFileInner {
    Memory(Bytes),
    Disk(NamedTempFile),
}

/// An uploaded artifact stored either in memory (small files) or on disk (large files).
///
/// Cloning is cheap — it increments an [`Arc`] reference count without copying data.
#[derive(Clone)]
pub struct ArtifactFile(Arc<ArtifactFileInner>);

impl ArtifactFile {
    fn new_memory(data: Bytes) -> Self {
        Self(Arc::new(ArtifactFileInner::Memory(data)))
    }

    fn new_disk(f: NamedTempFile) -> Self {
        Self(Arc::new(ArtifactFileInner::Disk(f)))
    }

    /// Open a synchronous reader for the artifact data.
    pub fn open_std_reader(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        match &*self.0 {
            ArtifactFileInner::Memory(bytes) => Ok(Box::new(std::io::Cursor::new(bytes.clone()))),
            ArtifactFileInner::Disk(f) => Ok(Box::new(std::fs::File::open(f.path())?)),
        }
    }
}

impl std::fmt::Debug for ArtifactFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.0 {
            ArtifactFileInner::Memory(b) => write!(f, "ArtifactFile::Memory({} bytes)", b.len()),
            ArtifactFileInner::Disk(file) => write!(f, "ArtifactFile::Disk({:?})", file.path()),
        }
    }
}

// ---------------------------------------------------------------------------
// UploadStore
// ---------------------------------------------------------------------------

/// Shared state between the HTTP server and the job handlers.
pub struct UploadStore {
    jobs: BTreeMap<String, Arc<Mutex<JobArtifactsInner>>>,
}

impl UploadStore {
    fn new() -> Self {
        Self {
            jobs: BTreeMap::new(),
        }
    }

    /// Called by the axum handler to store an uploaded file.
    ///
    /// Returns [`UploadError::InvalidPath`] if `path` fails sanitization,
    /// [`UploadError::UnknownKey`] if no job is registered under `key`, or
    /// [`UploadError::LimitExceeded`] if the per-job quota would be exceeded.
    pub fn upload_file(&mut self, key: &str, path: &str, data: Bytes) -> Result<(), UploadError> {
        let sanitized = sanitize_artifact_path(path).ok_or(UploadError::InvalidPath)?;
        if let Some(ja) = self.jobs.get(key) {
            ja.lock().unwrap().upload_artifact(&sanitized, data)
        } else {
            warn!("Attempt to upload {:?} for non-existent job key", path);
            Err(UploadError::UnknownKey)
        }
    }
}

// ---------------------------------------------------------------------------
// UploadServer
// ---------------------------------------------------------------------------

pub struct UploadServer {
    base_url: Option<url::Url>,
    store: Arc<Mutex<UploadStore>>,
}

impl UploadServer {
    pub fn new() -> Self {
        Self {
            base_url: None,
            store: Arc::new(Mutex::new(UploadStore::new())),
        }
    }

    /// Set the externally-routable base URL for the upload server.
    /// E.g. `"http://my-upload-host:2456/artifacts"`
    pub fn set_base_url(&mut self, base_url: url::Url) {
        self.base_url = Some(base_url);
    }

    pub fn store(&self) -> Arc<Mutex<UploadStore>> {
        self.store.clone()
    }

    /// Create a new [`JobArtifacts`] registration. The returned value is owned by `Run`.
    /// When it is dropped, its key is automatically removed from the store.
    pub fn add_new_job(&mut self) -> Option<JobArtifacts> {
        let base_url = self.base_url.as_ref()?;
        let key = generate_unique_id();
        let mut upload_url = base_url.clone();
        upload_url.path_segments_mut().ok()?.push(&key);
        // Ensure trailing slash so relative path resolution works correctly.
        let upload_url_str = format!("{}/", upload_url);

        let inner = Arc::new(Mutex::new(JobArtifactsInner::new()));
        self.store
            .lock()
            .unwrap()
            .jobs
            .insert(key.clone(), inner.clone());
        Some(JobArtifacts {
            key,
            upload_url: upload_url_str,
            inner,
            store: self.store.clone(),
        })
    }
}

fn generate_unique_id() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------------------
// JobArtifacts — RAII handle owned by Run
// ---------------------------------------------------------------------------

/// Owned by `Run`. Dropping it automatically deregisters the job key from the store.
pub struct JobArtifacts {
    key: String,
    upload_url: String,
    inner: Arc<Mutex<JobArtifactsInner>>,
    store: Arc<Mutex<UploadStore>>,
}

impl JobArtifacts {
    pub fn upload_url(&self) -> &str {
        &self.upload_url
    }

    pub fn artifact_paths(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .artifacts
            .keys()
            .cloned()
            .collect()
    }

    pub fn artifact_file(&self, path: &str) -> Option<ArtifactFile> {
        self.inner
            .lock()
            .unwrap()
            .artifacts
            .get(path)
            .map(|(_, f)| f.clone())
    }
}

impl Drop for JobArtifacts {
    fn drop(&mut self) {
        self.store.lock().unwrap().jobs.remove(&self.key);
    }
}

// ---------------------------------------------------------------------------
// JobArtifactsInner
// ---------------------------------------------------------------------------

struct JobArtifactsInner {
    /// Maps sanitized path → (byte size, stored file).
    artifacts: BTreeMap<String, (u64, ArtifactFile)>,
    /// Running total of bytes currently stored (decreases on overwrite).
    total_bytes: u64,
}

impl JobArtifactsInner {
    fn new() -> Self {
        Self {
            artifacts: BTreeMap::new(),
            total_bytes: 0,
        }
    }

    fn upload_artifact(&mut self, path: &str, data: Bytes) -> Result<(), UploadError> {
        let data_len = data.len() as u64;
        // Subtract any existing file at this path from the running total so that
        // overwrites don't permanently consume quota for the old content.
        let old_len = self.artifacts.get(path).map(|(size, _)| *size).unwrap_or(0);
        let new_total = self
            .total_bytes
            .saturating_sub(old_len)
            .saturating_add(data_len);
        if new_total > ARTIFACT_JOB_LIMIT {
            return Err(UploadError::LimitExceeded);
        }
        // For files at or above the memory threshold, spill to a temp file to
        // avoid keeping large payloads in RAM.  Note: the incoming `data` buffer
        // is already fully in memory at this point (axum's `Bytes` extractor);
        // this avoids a second large heap allocation but does not eliminate the
        // first one.  A future improvement could stream the body directly to disk.
        let artifact_file = if data_len >= ARTIFACT_MEMORY_THRESHOLD {
            let mut f = NamedTempFile::new().map_err(UploadError::Io)?;
            f.write_all(&data).map_err(UploadError::Io)?;
            ArtifactFile::new_disk(f)
        } else {
            ArtifactFile::new_memory(data)
        };
        self.total_bytes = new_total;
        self.artifacts
            .insert(path.to_string(), (data_len, artifact_file));
        Ok(())
    }
}
