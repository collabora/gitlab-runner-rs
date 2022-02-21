use bytes::Bytes;
use log::warn;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{interval_at, Duration, Instant, Interval, MissedTickBehavior};
use tracing::instrument::WithSubscriber;
use tracing::Instrument;

use crate::client::{ArtifactWhen, Client, JobResponse, JobState};
use crate::job::{Job, JobData};
use crate::runlist::{RunList, RunListEntry};
use crate::uploader::Uploader;
use crate::{JobHandler, JobResult, Phase};

async fn run<F, J, Ret>(
    job: Job,
    client: Client,
    response: Arc<JobResponse>,
    process: F,
    build_dir: PathBuf,
) -> JobResult
where
    F: FnOnce(Job) -> Ret,
    J: JobHandler,
    Ret: Future<Output = Result<J, ()>>,
{
    if let Err(e) = tokio::fs::create_dir(&build_dir).await {
        job.trace(format!("Failed to remove build dir: {}", e));
        return Err(());
    }
    let mut handler = process(job).await?;

    let script = response.step(Phase::Script).ok_or(())?;
    // TODO handle timeout
    let script_result = handler.step(&script.script, Phase::Script).await;

    if let Some(after) = response.step(Phase::AfterScript) {
        /* gitlab ignores the after_script result; so do the same */
        let _ = handler.step(&after.script, Phase::AfterScript).await;
    }

    //let upload = match response.
    let upload = response.artifacts.get(0).map_or(false, |a| match a.when {
        ArtifactWhen::Always => true,
        ArtifactWhen::OnSuccess => script_result.is_ok(),
        ArtifactWhen::OnFailure => script_result.is_err(),
    });

    let r = if upload {
        let mut uploader = Uploader::new(client, response);
        let r = handler.upload_artifacts(&mut uploader).await;
        if r.is_ok() {
            uploader.upload().await.and(script_result)
        } else {
            r
        }
    } else {
        script_result
    };

    handler.cleanup().await;

    if let Err(e) = tokio::fs::remove_dir_all(build_dir).await {
        warn!("Failed to remove build dir: {}", e);
    }

    r
}

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) struct Run {
    client: Client,
    response: Arc<JobResponse>,
    log_offset: usize,
    interval: Interval,
    // Last time a communication to gitlab was done so it knows the job is alive
    last_alive: Instant,
    jobdata: RunListEntry<u64, JobData>,
}

impl Run {
    pub(crate) fn new(
        client: Client,
        response: JobResponse,
        run_list: &mut RunList<u64, JobData>,
    ) -> Self {
        let response = Arc::new(response);
        let now = Instant::now();
        let jobdata = JobData::new();
        let jobdata = run_list.insert(response.id, jobdata);
        Self {
            client,
            response,
            log_offset: 0,
            interval: Self::create_interval(now, Duration::from_secs(3)),
            last_alive: now,
            jobdata,
        }
    }

    fn create_interval(instant: Instant, period: Duration) -> Interval {
        let mut interval = interval_at(instant + period, period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval
    }

    async fn update(&self, state: JobState) -> Result<(), crate::client::Error> {
        self.client
            .update_job(self.response.id, &self.response.token, state)
            .await?;
        Ok(())
    }

    async fn send_trace(&mut self, buf: Bytes) -> Result<Option<Duration>, crate::client::Error> {
        assert!(!buf.is_empty());
        let len = buf.len();

        let reply = self
            .client
            .trace(
                self.response.id,
                &self.response.token,
                buf,
                self.log_offset,
                len,
            )
            .await?;
        self.log_offset += len;
        Ok(reply.trace_update_interval)
    }

    #[tracing::instrument(skip(self, process,build_dir),fields(gitlab.job=self.response.id))]
    pub(crate) async fn run<F, J, Ret>(&mut self, process: F, build_dir: PathBuf)
    where
        F: FnOnce(Job) -> Ret + Send + Sync + 'static,
        J: JobHandler + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let job = Job::new(
            self.client.clone(),
            self.response.clone(),
            build_dir.clone(),
            self.jobdata.clone(),
        );
        let join = tokio::spawn(
            run(
                job,
                self.client.clone(),
                self.response.clone(),
                process,
                build_dir,
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
                    if let Some(buf) = self.jobdata.split_trace() {
                        // TODO be resiliant against send errors
                        if let Ok(Some(interval)) = self.send_trace(buf).await {
                            if interval != self.interval.period() {
                                self.interval = Self::create_interval(now, interval);
                            }
                        }
                        self.last_alive = now;
                    } else if now - self.last_alive >  KEEPALIVE_INTERVAL {
                        // In case of errors another update will be sent at the next tick
                        let _ = self.update(JobState::Running).await;
                        self.last_alive = now;
                    }
                },
                r = &mut join => break r
            }
        };

        // Send the remaining trace buffer back to gitlab.
        if let Some(buf) = self.jobdata.split_trace() {
            let _ = self.send_trace(buf).await.ok();
        }

        let state = match result {
            Ok(Ok(_)) => JobState::Success,
            Ok(Err(_)) => JobState::Failed,
            Err(_) => JobState::Failed,
        };
        self.update(state).await.expect("Failed to update");
    }
}
