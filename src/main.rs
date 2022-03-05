use std::collections::HashSet;
use std::io::Read;
use std::time::Duration;

use futures::stream::TryStreamExt;
use futures::AsyncWriteExt;
use futures::StreamExt;
use gitlab_runner::job::Job;
use gitlab_runner::outputln;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{JobHandler, JobResult, Phase, Runner};
use lava_api::job::Health;
use lava_api::joblog::JobLogError;
use lava_api::{job, Lava};
use rand::random;
use serde::Deserialize;
use structopt::StructOpt;
use tokio::time::sleep;
use tracing::Level;
use tracing::{debug, info};
use tracing_subscriber::filter;
use tracing_subscriber::prelude::*;
use url::Url;

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
}

#[derive(Deserialize, Debug, Clone)]
struct MonitorJobs {
    jobids: Vec<i64>,
}

struct Run {
    lava: Lava,
    job: Job,
    url: Url,
    ids: Vec<i64>,
}

impl Run {
    fn new(lava: Lava, url: Url, job: Job) -> Self {
        Self {
            lava,
            url,
            job,
            ids: Vec::new(),
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

    async fn update_log(&self, id: i64, offset: &mut u64) {
        let mut log = self.lava.log(id).start(*offset).log();
        while let Some(entry) = log.next().await {
            match entry {
                Ok(entry) => {
                    outputln!("{} {:?}", entry.dt, entry.msg);
                    *offset += 1;
                }
                Err(JobLogError::NoData) => (),
                Err(JobLogError::ParseError(s, e)) => {
                    outputln!("Couldn't parse {} - {}", s.trim_end(), e);
                    *offset += 1;
                }
                Err(e) => {
                    debug!("failed to get update from log: {:?}", e);
                    break;
                }
            }
        }
    }

    async fn wait_for_jobs(&self, mut ids: HashSet<i64>) -> JobResult {
        let mut running = HashSet::new();
        let mut failures = false;
        let mut timeout = MonitorTimeout::new(Duration::from_secs(30), Duration::from_secs(600));
        loop {
            if ids.is_empty() {
                break;
            }

            let mut builder = self.lava.jobs();
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

    async fn follow_job(&self, id: i64) -> JobResult {
        let builder = self.lava.jobs().id(id);
        let mut offset = 0u64;
        loop {
            let mut jobs = builder.clone().query();
            match jobs.try_next().await {
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

    async fn command(&mut self, command: &str) -> JobResult {
        outputln!("> {}", command);
        let mut p = command.split_whitespace();
        if let Some(cmd) = p.next() {
            debug!("command: >{}<", cmd);
            match cmd {
                "submit" => {
                    if let Some(filename) = p.next() {
                        let data = self.find_file(filename).await?;
                        let definition = match String::from_utf8(data) {
                            Ok(data) => data,
                            Err(_) => {
                                outputln!("Job definition is not utf-8");
                                return Err(());
                            }
                        };
                        let ids = self.submit_definition(&definition).await?;
                        self.ids.extend(&ids);
                        self.follow_job(ids[0]).await
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
                        self.wait_for_jobs(ids).await
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
impl JobHandler for Run {
    async fn step(&mut self, script: &[String], _phase: Phase) -> JobResult {
        for command in script {
            self.command(command).await?;
        }

        Ok(())
    }

    async fn upload_artifacts(&mut self, upload: &mut Uploader) -> JobResult {
        outputln!("\n\nUploading logs:");
        for id in &self.ids {
            let filename = format!("{}_log.yaml", id);
            let mut file = upload.file(filename.clone()).await;
            outputln!("Uploading {}", filename);
            let mut log = self.lava.log(*id).raw();

            while let Some(bytes) = log.next().await {
                match bytes {
                    Ok(b) => {
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
        }
        Ok(())
    }
}

async fn new_job(job: Job) -> Result<impl JobHandler, ()> {
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

    let lava = match Lava::new(lava_url.value(), Some(lava_token.value().to_string())) {
        Ok(l) => l,
        Err(e) => {
            outputln!("Failed to setup lava: {}", e);
            return Err(());
        }
    };

    Ok(Run::new(lava, url, job))
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

    runner
        .run(new_job, 64)
        .await
        .expect("Couldn't pick up jobs");
}
