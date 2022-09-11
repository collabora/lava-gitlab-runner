use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use colored::{Color, Colorize};
use futures::stream::TryStreamExt;
use futures::AsyncWriteExt;
use futures::StreamExt;
use gitlab_runner::job::Job;
use gitlab_runner::outputln;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{CancellableJobHandler, JobResult, Phase, Runner};
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
use tracing::{debug, info};
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;
use url::Url;

mod throttled;
use throttled::{ThrottledLava, Throttler};

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

struct Run {
    lava: Arc<ThrottledLava>,
    job: Job,
    url: Url,
    masker: Masker,
    ids: Vec<i64>,
    cancel_behaviour: Option<JobCancelBehaviour>,
}

impl Run {
    fn new(
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
        let masker = Masker::new(&masked, MASK_PATTERN);

        Self {
            lava,
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
                                return Ok(());
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

    fn transform(&self, definition: String) -> Result<String, ()> {
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
                        let definition = match String::from_utf8(data) {
                            Ok(data) => self.transform(data)?,
                            Err(_) => {
                                outputln!("Job definition is not utf-8");
                                return Err(());
                            }
                        };
                        let ids = self.submit_definition(&definition).await?;
                        self.ids.extend(&ids);
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
impl CancellableJobHandler for Run {
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

    async fn upload_artifacts(&mut self, upload: &mut Uploader) -> JobResult {
        outputln!("\n\nUploading logs:");
        for id in &self.ids {
            let filename = format!("{}_log.yaml", id);
            let mut file = upload.file(filename.clone()).await;
            outputln!("Uploading {}", filename);
            let mut log = self.lava.log(*id).await.raw();

            let mut cm = self.masker.mask_chunks();
            while let Some(bytes) = log.next().await {
                match bytes {
                    Ok(b) => {
                        let b = cm.mask_chunk(&b);
                        if let Err(e) = file.write_all(&b).await {
                            outputln!("Failed to write to jog log file {}", e);
                        }
                    }
                    Err(e) => {
                        outputln!("Couldn't read log {}", e);
                        return Err(());
                    }
                }
            }
            {
                let b = cm.finish();
                if let Err(e) = file.write_all(&b).await {
                    outputln!("Failed to write to job log file {}", e);
                }
            }
        }
        Ok(())
    }
}

type LavaMap = Arc<Mutex<BTreeMap<(String, String), Arc<ThrottledLava>>>>;

lazy_static! {
    static ref LAVA_MAP: LavaMap = Arc::new(Mutex::new(BTreeMap::new()));
    static ref MAX_CONCURRENT_REQUESTS: Arc<Mutex<usize>> = Arc::new(Mutex::new(20));
}

async fn new_job(job: Job) -> Result<impl CancellableJobHandler, ()> {
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

    runner
        .run(new_job, 64)
        .await
        .expect("Couldn't pick up jobs");
}
