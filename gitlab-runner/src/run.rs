use bytes::Bytes;
use masker::{Masker, MatchData};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{interval_at, Duration, Instant, Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use tracing::warn;
use tracing::Instrument;

use crate::client::{ArtifactWhen, Client, JobResponse, JobState};
use crate::job::{Job, JobLog};
use crate::runlist::{RunList, RunListEntry};
use crate::uploader::Uploader;
use crate::CancellableJobHandler;
use crate::{JobResult, Phase};

const GITLAB_MASK: &str = "[MASKED]";
const GITLAB_TOKEN_SUFFIX_CHARS: &str =
    "-.0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz=";

async fn run<F, J, Ret>(
    job: Job,
    client: Client,
    response: Arc<JobResponse>,
    masker: Masker,
    process: F,
    build_dir: PathBuf,
    cancel_token: CancellationToken,
) -> JobResult
where
    F: FnOnce(Job) -> Ret,
    J: CancellableJobHandler,
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

    let upload = !cancel_token.is_cancelled()
        && response.artifacts.get(0).map_or(false, |a| match a.when {
            ArtifactWhen::Always => true,
            ArtifactWhen::OnSuccess => script_result.is_ok(),
            ArtifactWhen::OnFailure => script_result.is_err(),
        });

    let r = if upload {
        if let Ok(mut uploader) = Uploader::new(client, &build_dir, response, masker) {
            let r = handler.upload_artifacts(&mut uploader).await;
            if r.is_ok() {
                uploader.upload().await.and(script_result)
            } else {
                r
            }
        } else {
            warn!("Failed to create uploader");
            Err(())
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
        if buf.is_empty() {
            // It's convenient to permit this because if we are
            // masking, the masker may not have produced any output,
            // so we'd just end up doing the same test in every
            // caller, rather than once here.
            return None;
        }
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

    #[tracing::instrument(skip(self, process,build_dir),fields(gitlab.job=self.response.id))]
    pub(crate) async fn run<F, J, Ret>(&mut self, process: F, build_dir: PathBuf)
    where
        F: FnOnce(Job) -> Ret + Send + Sync + 'static,
        J: CancellableJobHandler + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let cancel_token = CancellationToken::new();

        let masked_variables = self
            .response
            .variables
            .iter()
            .filter(|(_, v)| v.masked)
            .map(|(_, v)| v.value.as_str())
            .collect::<Vec<_>>();
        let prefixes = self
            .response
            .features
            .iter()
            .flat_map(|x| x.token_mask_prefixes.iter())
            // This matches the behaviour of the gitlab runner, which
            // explicitly supports a maximum of 10 prefixes.
            .take(10)
            .map(|p| MatchData {
                prefix: p.trim().as_bytes(),
                suffix: GITLAB_TOKEN_SUFFIX_CHARS.as_bytes(),
                mask_prefix: false,
            })
            .collect::<Vec<_>>();

        let masker = Masker::new_with_match_data(&masked_variables, &prefixes, GITLAB_MASK);

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
                masker.clone(),
                process,
                build_dir,
                cancel_token.clone(),
            )
            .in_current_span()
            .with_current_subscriber(),
        );
        tokio::pin!(join);

        let mut cm = masker.mask_chunks();

        let result = loop {
            tokio::select! {
                _ = self.interval.tick() => {
                    // Compare against *now* rather then the tick returned instant as that is the
                    // deadline which might have been missed; especially when testing.
                    let now = Instant::now();
                    if let Some(buf) = self.joblog.split_trace() {
                        // TODO be resiliant against send errors
                        let buf = cm.mask_chunk(buf).into();
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
            let buf = cm.mask_chunk(buf).into();
            self.send_trace(buf, &cancel_token).await;
        }
        // Flush anything the masker was holding back
        let buf = cm.finish().into();
        self.send_trace(buf, &cancel_token).await;

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
