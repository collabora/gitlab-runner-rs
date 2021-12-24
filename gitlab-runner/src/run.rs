use bytes::Bytes;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Duration, Instant, Interval, MissedTickBehavior};

use crate::client::{ArtifactWhen, Client, JobResponse, JobState};
use crate::job::Job;
use crate::uploader::Uploader;
use crate::{JobHandler, JobResult, Phase};

async fn run<F, J, Ret>(
    job: Job,
    client: Client,
    response: Arc<JobResponse>,
    process: F,
) -> JobResult
where
    F: FnOnce(Job) -> Ret,
    J: JobHandler,
    Ret: Future<Output = Result<J, ()>>,
{
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

    r
}

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
struct RunHandler {
    client: Client,
    response: Arc<JobResponse>,
    log_offset: usize,
    interval: Interval,
    // Last time a communication to gitlab was done so it knows the job is alive
    last_alive: Instant,
}

impl RunHandler {
    fn new(client: Client, response: JobResponse) -> Self {
        let response = Arc::new(response);
        let now = Instant::now();
        Self {
            client,
            response,
            log_offset: 0,
            interval: Self::create_interval(now, Duration::from_secs(3)),
            last_alive: now,
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

    async fn run<F, J, Ret>(&mut self, process: F)
    where
        F: FnOnce(Job) -> Ret + Send + Sync + 'static,
        J: JobHandler + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let (job, jobdata) = Job::new(self.client.clone(), self.response.clone());
        let join = tokio::spawn(run(
            job,
            self.client.clone(),
            self.response.clone(),
            process,
        ));
        tokio::pin!(join);

        let result = loop {
            tokio::select! {
                _ = self.interval.tick() => {
                    // Compare against *now* rather then the tick returned instant as that is the
                    // deadline which might have been missed; especially when testing.
                    let now = Instant::now();
                    if let Some(buf) = jobdata.split_trace() {
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
        if let Some(buf) = jobdata.split_trace() {
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

#[pin_project]
#[derive(Debug)]
pub(crate) struct Run {
    #[pin]
    handle: JoinHandle<()>,
}

impl Run {
    pub fn new<F, J, Ret>(process: F, client: Client, response: JobResponse) -> Self
    where
        F: FnOnce(Job) -> Ret + Sync + Send + 'static,
        J: JobHandler + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        // Spawn the processing in another async task to cope with it paniccing
        let mut handler = RunHandler::new(client, response);

        let handle = tokio::spawn(async move { handler.run(process).await });
        Self { handle }
    }
}

impl Future for Run {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().handle.poll(cx).map(|_| ())
    }
}
