use std::collections::HashSet;
use std::io::Read;
use std::time::Duration;

use futures::stream::TryStreamExt;
use futures::AsyncWriteExt;
use futures::StreamExt;
use gitlab_runner::job::Job;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{JobHandler, JobResult, Phase, Runner};
use lava_api::job::Health;
use lava_api::joblog::JobLogError;
use lava_api::{job, Lava};
use log::{debug, info};
use serde::Deserialize;
use structopt::StructOpt;
use tokio::time::sleep;
use url::Url;

#[derive(StructOpt)]
struct Opts {
    #[structopt(env = "GITLAB_URL")]
    server: Url,
    #[structopt(env = "GITLAB_TOKEN")]
    token: String,
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
                    self.job
                        .trace(format!("Failed to get artifact from {}", d.name()))
                        .await;
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
                        self.job
                            .trace(format!("Failed to read file from artifact: {}", e))
                            .await;
                        Err(())
                    }
                };
            }
        }
        self.job
            .trace(format!("{} not found in artifacts", filename))
            .await;
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
                    self.job
                        .trace(format!("Scheduled job: {}\n\n", self.url_for_id(*i)))
                        .await;
                }
                Ok(ids)
            }
            Err(e) => {
                self.job
                    .trace(format!("Failed to submit job: {:?}", e))
                    .await;
                Err(())
            }
        }
    }

    async fn update_log(&self, id: i64, offset: &mut u64) {
        let mut log = self.lava.log(id).start(*offset).log();
        while let Some(entry) = log.next().await {
            match entry {
                Ok(entry) => {
                    self.job
                        .trace(format!("{} {:?}\n", entry.dt, entry.msg))
                        .await;
                    *offset += 1;
                }
                Err(JobLogError::NoData) => (),
                Err(JobLogError::ParseError(s, e)) => {
                    self.job
                        .trace(format!("Couldn't parse {} - {}\n", s.trim_end(), e))
                        .await;
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
        loop {
            let mut builder = self.lava.jobs();
            for id in &ids {
                builder = builder.id(*id);
            }

            let mut jobs = builder.query();
            while let Some(job) = jobs.next().await {
                if let Ok(job) = job {
                    if job.state == job::State::Running && !running.contains(&job.id) {
                        self.job
                            .trace(format!("{} is running\n", self.url_for_id(job.id)))
                            .await;
                        running.insert(job.id);
                    }
                    if job.state == job::State::Finished {
                        ids.remove(&job.id);
                        if job.health == Health::Complete {
                            self.job
                                .trace(format!(
                                    "{} successfully finished\n",
                                    self.url_for_id(job.id)
                                ))
                                .await;
                        } else {
                            self.job
                                .trace(format!("{} UNSUCCESSFULL!\n", self.url_for_id(job.id)))
                                .await;
                            failures = true;
                        }
                    }
                }
            }

            if ids.is_empty() {
                break;
            }

            sleep(Duration::from_secs(20)).await;
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
                                self.job.trace("Job didn't complete correctly\n").await;
                                return Err(());
                            }
                        }
                        _ => (),
                    }
                    self.update_log(id, &mut offset).await;
                }
                Ok(None) => {
                    self.job.trace("Lava doesn't know about our job?").await;
                    return Err(());
                }
                Err(e) => {
                    self.job
                        .trace(format!("Failed to check status: {:?}", e))
                        .await;
                }
            }

            sleep(Duration::from_secs(20)).await;
        }
    }

    async fn command(&mut self, command: &str) -> JobResult {
        self.job.trace(format!("> {}\n", command)).await;
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
                                self.job.trace("Job definition is not utf-8").await;
                                return Err(());
                            }
                        };
                        let ids = self.submit_definition(&definition).await?;
                        self.ids.extend(&ids);
                        self.follow_job(ids[0]).await
                    } else {
                        self.job.trace("Missing file to submit").await;
                        Err(())
                    }
                }
                "monitor-file" => {
                    if let Some(filename) = p.next() {
                        let data = self.find_file(filename).await?;
                        let jobs: MonitorJobs = match serde_json::from_slice(&data) {
                            Ok(jobs) => jobs,
                            Err(e) => {
                                self.job
                                    .trace(format!("Failed to pars job file: {}", e))
                                    .await;
                                return Err(());
                            }
                        };
                        self.ids.extend(&jobs.jobids);
                        let mut ids = HashSet::new();
                        ids.extend(&jobs.jobids);

                        self.job.trace("Waiting for jobs:\n").await;
                        for id in &ids {
                            self.job
                                .trace(format!("\t* {}\n\n", self.url_for_id(*id)))
                                .await;
                        }
                        self.job.trace("\n").await;
                        self.wait_for_jobs(ids).await
                    } else {
                        self.job.trace("Missing file to submit").await;
                        Err(())
                    }
                }
                _ => {
                    self.job.trace("Unknown command\n").await;
                    Err(())
                }
            }
        } else {
            self.job.trace("empty command").await;
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
        self.job.trace("\n\nUploading logs:\n").await;
        for id in &self.ids {
            let filename = format!("{}_log.yaml", id);
            let mut file = upload.file(filename.clone()).await;
            self.job.trace(format!("Uploading {}\n", filename)).await;
            let mut log = self.lava.log(*id).raw();

            while let Some(bytes) = log.next().await {
                match bytes {
                    Ok(b) => {
                        if let Err(e) = file.write_all(&b).await {
                            self.job
                                .trace(format!("Failed to write to jog log file {}\n", e))
                                .await;
                        }
                    }
                    Err(e) => {
                        self.job.trace(format!("Couldn't read log {}\n", e)).await;
                        return Err(());
                    }
                }
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let opts = Opts::from_args();
    let mut runner = Runner::new(opts.server, opts.token);
    runner
        .run(
            move |job| async move {
                let lava_url = match job.variable("LAVA_URL") {
                    Some(u) => u,
                    None => {
                        job.trace("Missing LAVA_URL").await;
                        return Err(());
                    }
                };

                let lava_token = match job.variable("LAVA_TOKEN") {
                    Some(t) => t,
                    None => {
                        job.trace("Missing LAVA_TOKEN").await;
                        return Err(());
                    }
                };

                let url = match lava_url.value().parse() {
                    Ok(u) => u,
                    Err(e) => {
                        job.trace(format!("LAVA_URL is invalid: {}", e)).await;
                        return Err(());
                    }
                };

                let lava = match Lava::new(lava_url.value(), Some(lava_token.value().to_string())) {
                    Ok(l) => l,
                    Err(e) => {
                        job.trace(format!("Failed to setup lava: {}", e)).await;
                        return Err(());
                    }
                };

                info!("Created run");
                Ok(Run::new(lava, url, job))
            },
            64,
        )
        .await
        .expect("Couldn't pick up jobs");
}
