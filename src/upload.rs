use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tracing::warn;

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
    pub fn upload_file(&mut self, key: &str, path: &str, data: Bytes) {
        if let Some(ja) = self.jobs.get(key) {
            ja.lock().unwrap().upload_artifact(path, data);
        } else {
            warn!(
                "Ignoring attempt to upload {} for non-existent job key",
                path
            );
        }
    }
}

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
    /// E.g. "http://my-upload-host:2456/artifacts"
    pub fn set_base_url(&mut self, base_url: url::Url) {
        self.base_url = Some(base_url);
    }

    pub fn store(&self) -> Arc<Mutex<UploadStore>> {
        self.store.clone()
    }

    /// Create a new JobArtifacts registration. The returned value is owned by Run.
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

/// Owned by Run. Dropped when Run is done — automatically deregisters from the store.
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

    pub fn artifact_data(&self, path: &str) -> Option<Bytes> {
        self.inner
            .lock()
            .unwrap()
            .artifacts
            .get(path)
            .cloned()
    }
}

impl Drop for JobArtifacts {
    fn drop(&mut self) {
        self.store.lock().unwrap().jobs.remove(&self.key);
    }
}

struct JobArtifactsInner {
    artifacts: BTreeMap<String, Bytes>,
}

impl JobArtifactsInner {
    fn new() -> Self {
        Self {
            artifacts: BTreeMap::new(),
        }
    }

    fn upload_artifact(&mut self, path: &str, data: Bytes) {
        self.artifacts.insert(path.to_string(), data);
    }
}
