use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path;
use axum::routing::post;
use axum::Router;
use bytes::{Buf, Bytes};
use colored::{Color, Colorize};
use futures::stream::{Stream, TryStreamExt};
use futures::{AsyncRead, AsyncReadExt, FutureExt, StreamExt};
use gitlab_runner::job::Job;
use gitlab_runner::outputln;
use gitlab_runner::{CancellableJobHandler, JobResult, Phase, Runner, UploadableFile};
use handlebars::Handlebars;
use lava_api::job::Health;
use lava_api::joblog::{JobLogError, JobLogLevel, JobLogMsg};
use lava_api::paginator::PaginationError;
use lava_api::{job, Lava};
use lazy_static::lazy_static;
use masker::Masker;
use rand::random;
use serde::{Deserialize, Serialize};
use structopt::StructOpt;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing::{debug, info, warn};
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;
use url::Url;

mod throttled;
mod upload;

use throttled::{ThrottledLava, Throttler};
use upload::{JobArtifacts, UploadServer};

const MASK_PATTERN: &str = "[MASKED]";

#[derive(Debug, Clone)]
struct MonitorTimeout {
    next: Duration,
    max: Duration,
}

impl MonitorTimeout {
    fn new(initial: Duration, max: Duration) -> Self {
        MonitorTimeout { next: initial, max }
    }

    fn next_timeout(&mut self) -> Duration {
        self.next().unwrap()
    }
}

impl Iterator for MonitorTimeout {
    type Item = Duration;

    fn next(&mut self) -> Option<Self::Item> {
        let t = self.next.min(self.max);
        self.next = self.max.min(t * 2);

        /* Give a 25% random interval */
        let delta = t / 4;
        Some(t - delta / 2 + delta.mul_f32(random::<f32>()))
    }
}

#[test]
fn monitor_timeout() {
    let max = Duration::from_secs(600);
    let m = MonitorTimeout::new(Duration::from_secs(1), max);

    for (i, current) in m.take(20).enumerate() {
        // The expectation is that for each iteration the timeout median doubles and we start from
        // 1 second; maxing out at the maximum value
        let expected = Duration::from_secs(2u64.pow(i as u32)).min(max);
        assert!(
            current >= expected.mul_f32(0.75) && current < expected.mul_f32(1.25),
            "expected {:?} (+/- 25%), actual {:?}",
            expected,
            current
        );
    }
}

#[derive(StructOpt)]
struct Opts {
    #[structopt(env = "GITLAB_URL")]
    server: Url,
    #[structopt(env = "GITLAB_TOKEN")]
    token: String,
    #[structopt(short, long, env = "RUNNER_LOG")]
    log: Option<String>,
    #[structopt(
        short,
        long,
        default_value = "4",
        env = "RUNNER_MAX_CONCURRENT_REQUESTS"
    )]
    max_concurrent_requests: usize,
}

#[derive(Deserialize, Debug, Clone)]
struct MonitorJobs {
    jobids: Vec<i64>,
}

#[derive(Clone, Debug, Serialize)]
struct TransformVariables<'a> {
    pub job: BTreeMap<&'a str, &'a str>,
    pub runner: BTreeMap<&'a str, &'a str>,
}

#[derive(Debug)]
struct DisplayBox {
    width: usize,
    fg: Color,
}

impl DisplayBox {
    pub fn new(width: usize, fg: Color) -> Self {
        Self::edge(width, true, fg);
        Self { width, fg }
    }

    pub fn line<S, T>(&self, key: S, value: T)
    where
        S: AsRef<str>,
        T: AsRef<str>,
    {
        let kl = key.as_ref().len();
        let vl = value.as_ref().len();
        let total = kl + vl + 6; // two vertical bars, two edge spaces, colon space in centre
        let spacing = if total < self.width {
            self.width - total
        } else {
            0
        };
        let mut spacer = String::new();
        for _ in 0..spacing {
            spacer.push(' ');
        }
        let line = format!("| {}: {}{} |", key.as_ref(), value.as_ref(), spacer).color(self.fg);
        outputln!("{}", line);
    }

    pub fn end(self) {
        Self::edge(self.width, false, self.fg);
    }

    fn edge(width: usize, top: bool, fg: Color) {
        let mut line = String::new();
        let hyphens = if width > 2 { width - 2 } else { 0 };
        if top {
            line.push('/');
        } else {
            line.push('\\');
        }

        for _ in 0..hyphens {
            line.push('-');
        }

        if top {
            line.push('\\');
        } else {
            line.push('/');
        }
        outputln!("{}", line.color(fg));
    }
}

fn format_value(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Null => "null".to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => format!("\"{}\"", s),
        serde_yaml::Value::Sequence(seq) => {
            let mut s = String::new();
            s.push_str("[ ");
            let mut first = true;
            for item in seq.iter() {
                if !first {
                    s.push_str(", ");
                    first = false;
                }
                s.push_str(&format_value(item));
            }
            s.push_str(" ]");
            s
        }
        serde_yaml::Value::Mapping(map) => {
            let mut s = String::new();
            s.push_str("{ ");
            let mut first = true;
            for (k, v) in map.iter() {
                if !first {
                    s.push_str(", ");
                    first = false;
                }
                s = format!("{}{}: {}", s, format_value(k), format_value(v));
            }
            s.push_str(" }");
            s
        }
        serde_yaml::Value::Tagged(t) => format!("{}: {}", t.tag, format_value(&t.value)),
    }
}

fn abbreviate_level(lvl: &JobLogLevel) -> &'static str {
    match lvl {
        JobLogLevel::Debug => "debug =>",
        JobLogLevel::Info => " info =>",
        JobLogLevel::Warning => " WARN =>",
        JobLogLevel::Error => "ERROR =>",
        JobLogLevel::Results => "  res =>",
        JobLogLevel::Target => "  OUT =>",
        JobLogLevel::Input => "   IN <=",
        JobLogLevel::Feedback => "fdbak =>",
        JobLogLevel::Exception => "EXCPT =>",
    }
}

fn color_for_level(lvl: &JobLogLevel) -> Color {
    match lvl {
        JobLogLevel::Debug => Color::Cyan,
        JobLogLevel::Info => Color::BrightBlue,
        JobLogLevel::Warning => Color::Yellow,
        JobLogLevel::Error => Color::Red,
        JobLogLevel::Results => Color::Green,
        JobLogLevel::Target => Color::White,
        JobLogLevel::Input => Color::BrightMagenta,
        JobLogLevel::Feedback => Color::Green,
        JobLogLevel::Exception => Color::BrightRed,
    }
}

#[derive(Clone, Copy, Debug)]
enum JobCancelBehaviour {
    CancelLava,
    LeaveRunning,
}

struct AvailableArtifactStore {
    lava: Arc<ThrottledLava>,
    masker: Arc<Masker>,
    artifact_caches: Mutex<BTreeMap<String, Arc<Mutex<JobArtifacts>>>>,
    job_map: Mutex<BTreeMap<i64, Arc<Mutex<JobArtifacts>>>>,
}

impl AvailableArtifactStore {
    pub fn new(lava: Arc<ThrottledLava>, masker: Arc<Masker>) -> Self {
        Self {
            lava,
            masker,
            artifact_caches: Default::default(),
            job_map: Default::default(),
        }
    }

    pub fn get_log(
        &self,
        id: i64,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + '_ {
        self.masker.mask_stream(Box::pin(
            self.lava
                .log(id)
                .map(|x| x.raw())
                .flatten_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        ))
    }

    pub fn get_junit(
        &self,
        id: i64,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin + Send + '_ {
        Box::pin(
            self.lava
                .job_results_as_junit(id)
                .map(|res| match res {
                    Ok(s) => s
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                        .boxed(),
                    Err(e) => futures::stream::once(async move {
                        Err(std::io::Error::new(std::io::ErrorKind::Other, e))
                    })
                    .boxed(),
                })
                .flatten_stream(),
        )
    }

    pub fn create_upload_url(&self) -> String {
        let artifacts = UPLOAD_SERVER.lock().unwrap().add_new_job();
        let url = artifacts.lock().unwrap().get_upload_url().to_string();
        self.artifact_caches
            .lock()
            .unwrap()
            .insert(url.clone(), artifacts);
        url
    }

    pub fn add_job_for_upload_url(&self, id: i64, upload_url: &str) {
        let artifacts = self
            .artifact_caches
            .lock()
            .unwrap()
            .get(upload_url)
            .unwrap()
            .clone();
        self.job_map.lock().unwrap().insert(id, artifacts);
    }

    pub fn get_uploaded_artifact(&self, id: i64, path: &str) -> Option<Bytes> {
        self.job_map
            .lock()
            .unwrap()
            .get(&id)
            .and_then(|cache| cache.lock().unwrap().get_artifact_data(path))
    }

    pub fn get_uploaded_artifact_paths(&self, id: i64) -> Option<Vec<String>> {
        self.job_map.lock().unwrap().get(&id).map(|cache| {
            cache
                .lock()
                .unwrap()
                .get_artifact_paths()
                .map(str::to_string)
                .collect()
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
enum LavaUploadableFileType {
    Log { id: i64 },
    Junit { id: i64 },
    Artifact { id: i64, path: String },
}

#[derive(Clone)]
struct LavaUploadableFile {
    store: Arc<AvailableArtifactStore>,
    which: LavaUploadableFileType,
}

impl core::fmt::Debug for LavaUploadableFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        self.which.fmt(f)
    }
}

impl core::cmp::PartialEq for LavaUploadableFile {
    fn eq(&self, other: &Self) -> bool {
        self.which.eq(&other.which)
    }
}

impl core::cmp::Eq for LavaUploadableFile {}

impl core::cmp::PartialOrd for LavaUploadableFile {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl core::cmp::Ord for LavaUploadableFile {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.which.cmp(&other.which)
    }
}

impl LavaUploadableFile {
    pub fn log(id: i64, store: Arc<AvailableArtifactStore>) -> Self {
        Self {
            which: LavaUploadableFileType::Log { id },
            store,
        }
    }

    pub fn junit(id: i64, store: Arc<AvailableArtifactStore>) -> Self {
        Self {
            which: LavaUploadableFileType::Junit { id },
            store,
        }
    }

    pub fn artifact(id: i64, path: String, store: Arc<AvailableArtifactStore>) -> Self {
        Self {
            which: LavaUploadableFileType::Artifact { id, path },
            store,
        }
    }
}

impl UploadableFile for LavaUploadableFile {
    type Data<'a> = Box<dyn AsyncRead + Send + Unpin + 'a>;

    fn get_path(&self) -> Cow<'_, str> {
        match &self.which {
            LavaUploadableFileType::Log { id } => format!("{}_log.yaml", id).into(),
            LavaUploadableFileType::Junit { id } => format!("{}_junit.xml", id).into(),
            LavaUploadableFileType::Artifact { id, path } => {
                format!("{}_artifacts/{}", id, path).into()
            }
        }
    }

    fn get_data(&self) -> Self::Data<'_> {
        outputln!("Uploading {}", self.get_path());
        match &self.which {
            LavaUploadableFileType::Log { id } => {
                Box::new(self.store.get_log(*id).into_async_read())
            }
            LavaUploadableFileType::Junit { id } => {
                Box::new(self.store.get_junit(*id).into_async_read())
            }
            LavaUploadableFileType::Artifact { id, path } => Box::new(futures::io::Cursor::new(
                self.store.get_uploaded_artifact(*id, path).unwrap(),
            )),
        }
    }
}

struct Run {
    lava: Arc<ThrottledLava>,
    store: Arc<AvailableArtifactStore>,
    job: Job,
    url: Url,
    masker: Arc<Masker>,
    ids: Vec<i64>,
    cancel_behaviour: Option<JobCancelBehaviour>,
}

impl Run {
    pub fn new(
        lava: Arc<ThrottledLava>,
        url: Url,
        job: Job,
        cancel_behaviour: Option<JobCancelBehaviour>,
    ) -> Self {
        let masked = job
            .variables()
            .filter(|v| v.masked())
            .map(|v| v.value())
            .collect::<Vec<_>>();
        let masker = Arc::new(Masker::new(&masked, MASK_PATTERN));

        Self {
            lava: lava.clone(),
            store: Arc::new(AvailableArtifactStore::new(lava, masker.clone())),
            url,
            job,
            masker,
            ids: Vec::new(),
            cancel_behaviour,
        }
    }

    async fn find_file(&self, filename: &str) -> Result<Vec<u8>, ()> {
        for d in self.job.dependencies() {
            let artifact = match d.download().await {
                Ok(a) => a,
                Err(_) => {
                    outputln!("Failed to get artifact from {}", d.name());
                    continue;
                }
            };
            if let Some(mut artifact) = artifact {
                let data = if let Some(mut file) = artifact.file(filename) {
                    let mut data = Vec::new();
                    file.read_to_end(&mut data).map(|_| data)
                } else {
                    continue;
                };

                return match data {
                    Ok(data) => Ok(data),
                    Err(e) => {
                        outputln!("Failed to read file from artifact: {}", e);
                        Err(())
                    }
                };
            }
        }
        outputln!("{} not found in artifacts", filename);
        Err(())
    }

    fn url_for_id(&self, id: i64) -> Url {
        let mut url = self.url.clone();
        url.path_segments_mut()
            .unwrap()
            .push("scheduler")
            .push("job")
            .push(&id.to_string());
        url
    }

    async fn submit_definition(&self, definition: &str) -> Result<Vec<i64>, ()> {
        match self.lava.submit_job(definition).await {
            Ok(ids) => {
                for i in &ids {
                    outputln!("Scheduled job: {}", self.url_for_id(*i));
                }
                Ok(ids)
            }
            Err(e) => {
                outputln!("Failed to submit job: {:?}", e);
                Err(())
            }
        }
    }

    fn mask_variables(&self, msg: &str) -> String {
        self.masker.mask_str(msg)
    }

    async fn update_log(&self, id: i64, offset: &mut u64) {
        let mut log = self.lava.log(id).await.start(*offset).log();
        while let Some(entry) = log.next().await {
            match entry {
                Ok(entry) => {
                    match entry.msg {
                        JobLogMsg::Msg(s) => {
                            let fg = color_for_level(&entry.lvl);
                            outputln!(
                                "{} {}",
                                entry.dt.format("%Y-%m-%d %H:%M:%S%.6f"),
                                format!(
                                    "{} {}",
                                    abbreviate_level(&entry.lvl),
                                    self.mask_variables(&s)
                                )
                                .color(fg)
                            );
                        }
                        JobLogMsg::Msgs(ss) => {
                            let fg = color_for_level(&entry.lvl);
                            for s in ss {
                                outputln!(
                                    "{} {}",
                                    entry.dt.format("%Y-%m-%d %H:%M:%S%.6f"),
                                    format!(
                                        "{} {}",
                                        abbreviate_level(&entry.lvl),
                                        self.mask_variables(&s)
                                    )
                                    .color(fg)
                                );
                            }
                        }
                        JobLogMsg::Result(res) => {
                            let fg = match res.result.as_str() {
                                "pass" => color_for_level(&JobLogLevel::Results),
                                "fail" => color_for_level(&JobLogLevel::Error),
                                _ => color_for_level(&JobLogLevel::Warning),
                            };

                            let b = DisplayBox::new(70, fg);
                            b.line("case", self.mask_variables(&res.case));
                            b.line("definition", self.mask_variables(&res.definition));
                            if let Some(ns) = res.namespace {
                                b.line("namespace", self.mask_variables(&ns));
                            }
                            if let Some(level) = res.level {
                                b.line("level", self.mask_variables(&level));
                            }
                            b.line("result", self.mask_variables(&res.result));
                            if let Some(duration) = res.duration {
                                b.line(
                                    "duration",
                                    format!(
                                        "{}.{:0>9}",
                                        duration.as_secs(),
                                        duration.subsec_nanos()
                                    ),
                                );
                            }
                            for (k, v) in res.extra.iter() {
                                b.line(
                                    self.mask_variables(k),
                                    self.mask_variables(&format_value(v)),
                                );
                            }
                            b.end();
                        }
                    }
                    *offset += 1;
                }
                Err(JobLogError::NoData) => (),
                Err(JobLogError::ParseError(s, e)) => {
                    outputln!(
                        "{}",
                        format!(
                            "Couldn't parse {} - {}",
                            self.mask_variables(s.trim_end()),
                            self.mask_variables(&e.to_string())
                        )
                        .bright_red()
                    );
                    *offset += 1;
                }
                Err(e) => {
                    debug!("failed to get update from log: {:?}", e);
                    break;
                }
            }
        }
    }

    async fn cancel_job(
        &self,
        default_behaviour: JobCancelBehaviour,
        job_id: i64,
    ) -> Result<(), job::CancellationError> {
        match self.cancel_behaviour.unwrap_or(default_behaviour) {
            JobCancelBehaviour::CancelLava => {
                info!("Cancelling LAVA job {}", job_id);
                self.lava.cancel_job(job_id).await
            }
            JobCancelBehaviour::LeaveRunning => {
                info!("Not cancelling LAVA job {}, leaving it running", job_id);
                Ok(())
            }
        }
    }

    async fn wait_for_jobs(
        &self,
        mut ids: HashSet<i64>,
        cancel_token: &CancellationToken,
        cancel_behaviour: JobCancelBehaviour,
    ) -> JobResult {
        let mut running = HashSet::new();
        let mut failures = false;
        let mut timeout = MonitorTimeout::new(Duration::from_secs(30), Duration::from_secs(600));
        loop {
            if ids.is_empty() {
                break;
            }

            let mut builder = self.lava.jobs().await;
            for id in &ids {
                builder = builder.id(*id);
            }

            let mut jobs = builder.query();
            while let Some(job) = jobs.next().await {
                if let Ok(job) = job {
                    if job.state == job::State::Running && !running.contains(&job.id) {
                        outputln!("{} is running", self.url_for_id(job.id));
                        running.insert(job.id);
                    }
                    if job.state == job::State::Finished {
                        ids.remove(&job.id);
                        if job.health == Health::Complete {
                            outputln!("{} successfully finished", self.url_for_id(job.id));
                        } else {
                            outputln!("{} UNSUCCESSFULL!", self.url_for_id(job.id));
                            failures = true;
                        }
                    }
                    if cancel_token.is_cancelled() {
                        match job.state {
                            job::State::Finished | job::State::Canceling => {}
                            _ => match self.cancel_job(cancel_behaviour, job.id).await {
                                Ok(_) => {
                                    outputln!("Cancelled LAVA job {}", job.id);
                                    ids.remove(&job.id);
                                }
                                Err(e) => {
                                    outputln!("Error cancelling LAVA job {}: {}", job.id, e);
                                }
                            },
                        }
                    }
                }
            }

            sleep(timeout.next_timeout()).await;
        }

        if failures {
            Err(())
        } else {
            Ok(())
        }
    }

    async fn all_tests_passed(&self, id: i64) -> Result<bool, ()> {
        let mut bytes = Vec::new();
        Box::pin(
            self.lava
                .job_results_as_junit(id)
                .await
                .map_err(|_| ())?
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        )
        .into_async_read()
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| {
            outputln!("Failed to get job results: {}", e);
        })?;

        let ts = junit_parser::from_reader(bytes.reader()).map_err(|e| {
            outputln!("Failed to parse job results: {}", e);
        })?;

        Ok(ts.errors == 0 && ts.failures == 0)
    }

    async fn get_job(&self, id: i64) -> Result<Option<job::Job>, PaginationError> {
        let mut jobs = self.lava.jobs().await.id(id).query();
        jobs.try_next().await
    }

    async fn follow_job(
        &self,
        id: i64,
        cancel_token: &CancellationToken,
        cancel_behaviour: JobCancelBehaviour,
    ) -> JobResult {
        let mut offset = 0u64;
        loop {
            // `get_job` is a separate function in order to limit the
            // scope of the permit it obtains via the throttled Lava
            // interface. This is important because within the loop we
            // will obtain a second permit for access to the logs for
            // this job. Owning two permits simultaneously can cause
            // deadlocks in this version of the throttling API.
            match self.get_job(id).await {
                Ok(Some(job)) => {
                    match job.state {
                        job::State::Running => self.update_log(id, &mut offset).await,
                        job::State::Finished => {
                            /* Get the final part of the log if any */
                            self.update_log(id, &mut offset).await;

                            if job.health == Health::Complete {
                                match self.all_tests_passed(id).await {
                                    Ok(true) => {
                                        return Ok(());
                                    }
                                    Ok(false) => {
                                        outputln!("Job completed with errors");
                                        return Err(());
                                    }
                                    Err(_) => return Err(()),
                                };
                            } else {
                                outputln!("Job didn't complete correctly");
                                return Err(());
                            }
                        }
                        _ => (),
                    }
                    self.update_log(id, &mut offset).await;
                    if cancel_token.is_cancelled() {
                        match job.state {
                            job::State::Finished | job::State::Canceling => {}
                            _ => match self.cancel_job(cancel_behaviour, id).await {
                                Ok(_) => {
                                    outputln!("Cancelled LAVA job {}", id);
                                    return Ok(());
                                }
                                Err(e) => {
                                    outputln!("Error cancelling LAVA job {}: {}", id, e);
                                }
                            },
                        }
                    }
                }
                Ok(None) => {
                    outputln!("Lava doesn't know about our job?");
                    return Err(());
                }
                Err(e) => {
                    outputln!("Failed to check status: {:?}", e);
                }
            }

            sleep(Duration::from_secs(20)).await;
        }
    }

    fn transform(&self, definition: String, upload_url: &str) -> Result<String, ()> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars
            .register_template_string("definition", definition)
            .map_err(|e| {
                outputln!("Failed to parse template: {}", e);
            })?;

        let mappings = TransformVariables {
            job: self
                .job
                .variables()
                .map(|var| (var.key(), var.value()))
                .collect(),
            runner: BTreeMap::from([("ARTIFACT_UPLOAD_URL", upload_url)]),
        };
        handlebars.render("definition", &mappings).map_err(|e| {
            outputln!("Failed to substitute in template: {}", e);
        })
    }

    async fn command(&mut self, command: &str, cancel_token: &CancellationToken) -> JobResult {
        outputln!("> {}", command);
        let mut p = command.split_whitespace();
        if let Some(cmd) = p.next() {
            debug!("command: >{}<", cmd);
            match cmd {
                "submit" => {
                    if let Some(filename) = p.next() {
                        let data = self.find_file(filename).await?;
                        let upload_url = self.store.create_upload_url();
                        let definition = match String::from_utf8(data) {
                            Ok(data) => self.transform(data, &upload_url)?,
                            Err(_) => {
                                outputln!("Job definition is not utf-8");
                                return Err(());
                            }
                        };
                        let ids = self.submit_definition(&definition).await?;
                        self.ids.extend(&ids);
                        for id in &self.ids {
                            self.store.add_job_for_upload_url(*id, &upload_url);
                        }
                        self.follow_job(ids[0], cancel_token, JobCancelBehaviour::CancelLava)
                            .await
                    } else {
                        outputln!("Missing file to submit");
                        Err(())
                    }
                }
                "monitor-file" => {
                    if let Some(filename) = p.next() {
                        let data = self.find_file(filename).await?;
                        let jobs = match serde_json::from_slice::<MonitorJobs>(&data) {
                            Ok(jobs) if jobs.jobids.is_empty() => {
                                outputln!("No job ids in json file!");
                                return Err(());
                            }
                            Ok(jobs) => jobs,
                            Err(e) => {
                                outputln!("Failed to parse job file: {}", e);
                                return Err(());
                            }
                        };
                        self.ids.extend(&jobs.jobids);
                        let mut ids = HashSet::new();
                        ids.extend(&jobs.jobids);

                        outputln!("Waiting for jobs:");
                        for id in &ids {
                            outputln!("\t* {}", self.url_for_id(*id));
                        }
                        outputln!("");
                        self.wait_for_jobs(ids, cancel_token, JobCancelBehaviour::LeaveRunning)
                            .await
                    } else {
                        outputln!("Missing file to submit");
                        Err(())
                    }
                }
                _ => {
                    outputln!("Unknown command");
                    Err(())
                }
            }
        } else {
            outputln!("empty command");
            Err(())
        }
    }
}

#[async_trait::async_trait]
impl CancellableJobHandler<LavaUploadableFile> for Run {
    async fn step(
        &mut self,
        script: &[String],
        _phase: Phase,
        cancel_token: &CancellationToken,
    ) -> JobResult {
        for command in script {
            self.command(command, cancel_token).await?;
        }

        Ok(())
    }

    async fn get_uploadable_files(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = LavaUploadableFile> + Send>, ()> {
        let mut available_files = Vec::new();
        for id in &self.ids {
            available_files.push(LavaUploadableFile::log(*id, self.store.clone()));
            available_files.push(LavaUploadableFile::junit(*id, self.store.clone()));
            for path in self
                .store
                .get_uploaded_artifact_paths(*id)
                .into_iter()
                .flatten()
            {
                available_files.push(LavaUploadableFile::artifact(*id, path, self.store.clone()));
            }
        }
        Ok(Box::new(available_files.into_iter()))
    }
}

type LavaMap = Arc<Mutex<BTreeMap<(String, String), Arc<ThrottledLava>>>>;

lazy_static! {
    static ref LAVA_MAP: LavaMap = Arc::new(Mutex::new(BTreeMap::new()));
    static ref MAX_CONCURRENT_REQUESTS: Arc<Mutex<usize>> = Arc::new(Mutex::new(20));
    static ref UPLOAD_SERVER: Arc<Mutex<UploadServer>> = Default::default();
}

async fn new_job(job: Job) -> Result<impl CancellableJobHandler<LavaUploadableFile>, ()> {
    info!("Creating new run for job: {}", job.id());
    let lava_url = match job.variable("LAVA_URL") {
        Some(u) => u,
        None => {
            outputln!("Missing LAVA_URL");
            return Err(());
        }
    };

    let lava_token = match job.variable("LAVA_TOKEN") {
        Some(t) => t,
        None => {
            outputln!("Missing LAVA_TOKEN");
            return Err(());
        }
    };

    let url = match lava_url.value().parse() {
        Ok(u) => u,
        Err(e) => {
            outputln!("LAVA_URL is invalid: {}", e);
            return Err(());
        }
    };

    let cancel_behaviour = match job
        .variable("LAVA_CANCELLATION")
        .map(|v| match v.value().to_lowercase().as_str() {
            "cancel" => Ok(JobCancelBehaviour::CancelLava),
            "ignore" => Ok(JobCancelBehaviour::LeaveRunning),
            _ => {
                outputln!(
                    "Bad value '{}' for LAVA_CANCELLATION: must be 'ignore' or 'cancel'",
                    v.value()
                );
                Err(())
            }
        })
        .transpose()
    {
        Ok(v) => v,
        Err(_) => return Err(()),
    };

    let max_requests = {
        let m = MAX_CONCURRENT_REQUESTS.lock().unwrap();
        *m
    };

    let lava = match LAVA_MAP
        .lock()
        .unwrap()
        .entry((lava_url.value().to_string(), lava_token.value().to_string()))
    {
        Entry::Occupied(o) => o.get().clone(),
        Entry::Vacant(v) => {
            match Lava::new(lava_url.value(), Some(lava_token.value().to_string())) {
                Ok(lava) => {
                    let throttled =
                        Arc::new(ThrottledLava::new(lava, Throttler::new(max_requests)));
                    v.insert(throttled.clone());
                    throttled
                }
                Err(e) => {
                    outputln!("Failed to setup lava: {}", e);
                    return Err(());
                }
            }
        }
    };

    Ok(Run::new(lava, url, job, cancel_behaviour))
}

async fn upload_artifact(Path((job, path)): Path<(String, String)>, body: Bytes) {
    UPLOAD_SERVER.lock().unwrap().upload_file(&job, &path, body);
}

#[tokio::main]
async fn main() {
    let opts = Opts::from_args();
    let dir = tempfile::tempdir().unwrap();

    let (mut runner, layer) =
        Runner::new_with_layer(opts.server, opts.token, dir.path().to_path_buf());

    let log_targets: filter::Targets = if let Some(log) = opts.log {
        log.parse().unwrap()
    } else {
        filter::Targets::new().with_default(Level::INFO)
    };

    tracing_subscriber::Registry::default()
        .with(layer)
        .with(tracing_subscriber::fmt::Layer::new().with_filter(log_targets))
        .init();

    {
        let mut max_requests = MAX_CONCURRENT_REQUESTS.lock().unwrap();
        *max_requests = opts.max_concurrent_requests;
        info!(
            "Setting max concurrent requests to {}",
            opts.max_concurrent_requests
        );
    }

    tokio::spawn(async {
        let local_port = match env::var("LAVA_GITLAB_RUNNER_LOCAL_PORT") {
            Ok(val) => val
                .parse()
                .expect("failed to parse LAVA_GITLAB_RUNNER_LOCAL_PORT as a port number"),
            Err(_) => {
                warn!("No LAVA_GITLAB_RUNNER_LOCAL_PORT set, will listen on ephemeral IP.");
                0u16
            }
        };

        let listener = std::net::TcpListener::bind(std::net::SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            local_port,
        ))
        .expect("failed to bind listener");

        let routable_host = match env::var("LAVA_GITLAB_RUNNER_ROUTABLE_HOST") {
            Ok(val) => val,
            Err(_) => {
                let host = local_ip_address::local_ip()
                    .expect("failed to determine local ip")
                    .to_string();

                warn!(
                    "No LAVA_GITLAB_RUNNER_ROUTABLE_HOST set, using best guess of local IP {}.",
                    host
                );
                host
            }
        };

        let routable_port = match env::var("LAVA_GITLAB_RUNNER_ROUTABLE_PORT") {
            Ok(val) => val
                .parse()
                .expect("failed to parse LAVA_GITLAB_RUNNER_ROUTABLE_PORT as a port number"),
            Err(_) => {
                let port = listener
                    .local_addr()
                    .expect("failed to get local address")
                    .port();

                info!(
                    "No LAVA_GITLAB_RUNNER_ROUTABLE_PORT set, using local port {}.",
                    port
                );
                port
            }
        };

        let routable_addr = format!("{}:{}", routable_host, routable_port);

        info!(
            "Artifact upload listening on {} (reporting routable {})",
            listener.local_addr().expect("failed to get local address"),
            routable_addr
        );

        UPLOAD_SERVER
            .lock()
            .unwrap()
            .set_base_address(routable_addr);
        let app = Router::new().route("/artifacts/:job/*path", post(upload_artifact));

        axum::Server::from_tcp(listener)
            .expect("failed to create axum server from TCP listener")
            .serve(app.into_make_service())
            .await
            .unwrap();
    });

    runner
        .run(new_job, 64)
        .await
        .expect("Couldn't pick up jobs");
}
