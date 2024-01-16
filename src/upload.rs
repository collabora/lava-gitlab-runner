use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use bytes::Bytes;
use rand::distributions::Alphanumeric;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use tracing::warn;

pub struct UploadServer {
    rng: ChaCha20Rng,
    job_cache: BTreeMap<String, Weak<Mutex<JobArtifacts>>>,
    base_addr: Option<String>,
}

impl UploadServer {
    pub fn new() -> Self {
        Self {
            rng: ChaCha20Rng::from_entropy(),
            job_cache: Default::default(),
            base_addr: None,
        }
    }

    pub fn add_new_job(&mut self) -> Arc<Mutex<JobArtifacts>> {
        // Wipe any dead jobs as the new one starts. It's not hugely
        // important when this happens, so long as it happens
        // periodically.
        self.cleanup();

        let prefix = self.generate_unique_id();
        let base_addr = self
            .base_addr
            .as_ref()
            .expect("failed to set base_address on UploadServer before adding jobs.");
        let url = format!("https://{}/artifacts/{}/", base_addr, prefix);
        let ja: Arc<Mutex<JobArtifacts>> = Arc::new(Mutex::new(JobArtifacts::new(url)));
        self.job_cache.insert(prefix, Arc::downgrade(&ja));
        ja
    }

    pub fn set_base_address(&mut self, base_addr: String) {
        self.base_addr = Some(base_addr);
    }

    pub fn upload_file(&mut self, key: &str, path: &str, data: Bytes) {
        if let Some(ja) = self.job_cache.get(key).and_then(Weak::upgrade) {
            ja.lock().unwrap().upload_artifact(path, data)
        } else {
            warn!(
                "Ignoring attempt to upload {} for non-existent or expired job",
                path
            );
        }
    }

    fn generate_unique_id(&mut self) -> String {
        (&mut self.rng)
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect()
    }

    pub fn cleanup(&mut self) {
        let mut cleaned = Vec::new();
        for (k, v) in self.job_cache.iter() {
            if v.strong_count() == 0 {
                cleaned.push(k.clone());
            }
        }
        for k in cleaned {
            self.job_cache.remove(&k);
        }
    }
}

impl Default for UploadServer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct JobArtifacts {
    artifacts: BTreeMap<String, Bytes>,
    url: String,
}

impl JobArtifacts {
    fn new(url: String) -> Self {
        JobArtifacts {
            artifacts: Default::default(),
            url,
        }
    }

    pub fn get_upload_url(&self) -> &str {
        &self.url
    }

    pub fn get_artifact_paths(&self) -> impl Iterator<Item = &str> {
        self.artifacts.iter().map(|x| x.0.as_ref())
    }

    pub fn get_artifact_data(&self, path: &str) -> Option<Bytes> {
        self.artifacts.get(path).map(Clone::clone)
    }

    pub fn upload_artifact(&mut self, path: &str, data: Bytes) {
        self.artifacts.insert(path.to_string(), data);
    }
}
