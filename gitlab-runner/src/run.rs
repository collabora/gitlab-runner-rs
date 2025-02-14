use bytes::Bytes;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{interval_at, Duration, Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use tracing::warn;
use tracing::Instrument;

use crate::client::{ArtifactWhen, Client, JobArtifact, JobResponse, JobState};
use crate::job::{Job, JobLog};
use crate::runlist::{RunList, RunListEntry};
use crate::uploader::Uploader;
use crate::{CancellableJobHandler, UploadableFile};
use crate::{JobResult, Phase};

async fn run<F, J, U, Ret>(
    job: Job,
    client: Client,
    response: Arc<JobResponse>,
    process: F,
    build_dir: PathBuf,
    cancel_token: CancellationToken,
) -> JobResult
where
    F: FnOnce(Job) -> Ret,
    J: CancellableJobHandler<U>,
    U: UploadableFile + Send + 'static,
    Ret: Future<Output = Result<J, ()>>,
{
    if let Err(e) = tokio::fs::create_dir(&build_dir).await {
        job.trace(format!("Failed to remove build dir: {}", e));
        return Err(());
    }
    let mut handler = process(job).await?;

    let script_result = if !cancel_token.is_cancelled() {
        let script = response.step(Phase::Script).ok_or(())?;
        let script_result = handler
            .step(&script.script, Phase::Script, &cancel_token)
            .await;

        if !cancel_token.is_cancelled() {
            if let Some(after) = response.step(Phase::AfterScript) {
                // gitlab ignores the after_script result; so do the same
                let _ = handler
                    .step(&after.script, Phase::AfterScript, &cancel_token)
                    .await;
            }
        }

        script_result
    } else {
        Ok(())
    };

    let mut overall_result = script_result;

    if !cancel_token.is_cancelled() {
        for artifact in response.artifacts.iter() {
            if process_artifact(
                artifact,
                script_result,
                client.clone(),
                build_dir.as_ref(),
                response.as_ref(),
                &mut handler,
            )
            .await
            .is_err()
            {
                overall_result = Err(());
                break;
            }
        }
    }

    handler.cleanup().await;

    if let Err(e) = tokio::fs::remove_dir_all(build_dir).await {
        warn!("Failed to remove build dir: {}", e);
    }

    overall_result
}

async fn process_artifact<J, U>(
    artifact: &JobArtifact,
    script_result: JobResult,
    client: Client,
    build_dir: &Path,
    response: &JobResponse,
    handler: &mut J,
) -> JobResult
where
    J: CancellableJobHandler<U>,
    U: UploadableFile + Send + 'static,
{
    let upload = match artifact.when {
        ArtifactWhen::Always => true,
        ArtifactWhen::OnSuccess => script_result.is_ok(),
        ArtifactWhen::OnFailure => script_result.is_err(),
    };

    if !upload {
        return Ok(());
    }

    let mut uploader = match Uploader::new(
        client,
        build_dir,
        response.id,
        response.token.clone(),
        artifact,
    ) {
        Ok(uploader) => uploader,
        Err(_) => {
            warn!("Failed to create uploader");
            return Err(());
        }
    };

    let mut uploaded = 0;

    for file in handler.get_uploadable_files().await? {
        if artifact
            .paths
            .iter()
            .any(|path| match glob::Pattern::new(path) {
                Ok(pattern) => pattern.matches(&file.get_path()),
                Err(_) => path == &file.get_path(),
            })
        {
            let path = file.get_path();
            match uploader.file(path.to_string()).await {
                Ok(mut upload) => {
                    let mut data = file.get_data().await?;
                    match futures::io::copy(&mut data, &mut upload).await {
                        Ok(_) => {
                            uploaded += 1;
                        }
                        Err(e) => {
                            warn!("Failed to upload file: {:?}", e);
                            return Err(());
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to begin new file upload: {:?}", e);
                    return Err(());
                }
            }
        }
    }

    if uploaded > 0 {
        if let Err(e) = uploader.upload().await {
            warn!("Failed to upload artifact: {:?}", e);
            return Err(());
        }
    }

    Ok(())
}

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) struct Run {
    client: Client,
    response: Arc<JobResponse>,
    log_offset: usize,
    interval: Interval,
    // Last time a communication to gitlab was done so it knows the job is alive
    last_alive: Instant,
    joblog: RunListEntry<u64, JobLog>,
}

impl Run {
    pub(crate) fn new(
        client: Client,
        response: JobResponse,
        run_list: &mut RunList<u64, JobLog>,
    ) -> Self {
        let response = Arc::new(response);
        let now = Instant::now();
        let joblog = JobLog::new();
        let joblog = run_list.insert(response.id, joblog);
        Self {
            client,
            response,
            log_offset: 0,
            interval: Self::create_interval(now, Duration::from_secs(3)),
            last_alive: now,
            joblog,
        }
    }

    fn create_interval(instant: Instant, period: Duration) -> Interval {
        let mut interval = interval_at(instant + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval
    }

    async fn update(&self, state: JobState, cancel_token: &CancellationToken) {
        match self
            .client
            .update_job(self.response.id, &self.response.token, state)
            .await
        {
            Ok(_reply) => (),
            Err(crate::client::Error::JobCancelled) => cancel_token.cancel(),
            Err(err) => warn!("Failed to update job status: {:?}", err),
        }
    }

    async fn send_trace(
        &mut self,
        buf: Bytes,
        cancel_token: &CancellationToken,
    ) -> Option<Duration> {
        assert!(!buf.is_empty());
        let len = buf.len();

        match self
            .client
            .trace(
                self.response.id,
                &self.response.token,
                buf,
                self.log_offset,
                len,
            )
            .await
        {
            Ok(reply) => {
                self.log_offset += len;
                reply.trace_update_interval
            }
            Err(crate::client::Error::JobCancelled) => {
                cancel_token.cancel();
                None
            }
            Err(err) => {
                warn!("Failed to send job trace: {:?}", err);
                None
            }
        }
    }

    // the gitlab_runner::gitlab::job target is specified for crate users to depend on; Also it's
    // at max verbosity (error), so it always shows up regardless of level.
    #[tracing::instrument(target = "gitlab_runner::gitlab::job",
                          level = "error",
                          skip_all,
                          fields(gitlab.job=self.response.id))]
    pub(crate) async fn run<F, J, U, Ret>(&mut self, process: F, build_dir: PathBuf)
    where
        F: FnOnce(Job) -> Ret + Send + Sync + 'static,
        J: CancellableJobHandler<U> + 'static,
        U: UploadableFile + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let cancel_token = CancellationToken::new();

        let job = Job::new(
            self.client.clone(),
            self.response.clone(),
            build_dir.clone(),
            self.joblog.clone(),
        );
        let join = tokio::spawn(
            run(
                job,
                self.client.clone(),
                self.response.clone(),
                process,
                build_dir,
                cancel_token.clone(),
            )
            .in_current_span()
            .with_current_subscriber(),
        );
        tokio::pin!(join);

        let result = loop {
            tokio::select! {
                _ = self.interval.tick() => {
                    // Compare against *now* rather then the tick returned instant as that is the
                    // deadline which might have been missed; especially when testing.
                    let now = Instant::now();
                    if let Some(buf) = self.joblog.split_trace() {
                        // TODO be resiliant against send errors
                        if let Some(interval) = self.send_trace(buf, &cancel_token).await {
                            if interval != self.interval.period() {
                                self.interval = Self::create_interval(now, interval);
                            }
                        }
                        self.last_alive = now;
                    } else if now - self.last_alive > KEEPALIVE_INTERVAL {
                        // In case of errors another update will be sent at the next tick
                        self.update(JobState::Running, &cancel_token).await;
                        self.last_alive = now;
                    }
                },
                r = &mut join => break r
            }
        };

        // Send the remaining trace buffer back to gitlab.
        if let Some(buf) = self.joblog.split_trace() {
            self.send_trace(buf, &cancel_token).await;
        }

        // Don't bother updating the status if cancelled, since it will just fail.
        if !cancel_token.is_cancelled() {
            let state = match result {
                Ok(Ok(_)) => JobState::Success,
                Ok(Err(_)) => JobState::Failed,
                Err(_) => JobState::Failed,
            };
            self.update(state, &cancel_token).await;
        }
    }
}
