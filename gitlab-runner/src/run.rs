use bytes::Bytes;
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::client::{ArtifactWhen, Client, JobResponse, JobState};
use crate::job::{Job, JobRequest};
use crate::uploader::Uploader;
use crate::{JobHandler, JobResult, Phase};

async fn run<F, J, Ret>(
    job: Job,
    client: Client,
    response: Arc<JobResponse>,
    process: F,
) -> JobResult
where
    F: Fn(Job) -> Ret,
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
    let upload = if let Some(a) = response.artifacts.get(0) {
        match a.when {
            ArtifactWhen::Always => true,
            ArtifactWhen::OnSuccess => script_result.is_ok(),
            ArtifactWhen::OnFailure => script_result.is_err(),
        }
    } else {
        false
    };

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

struct RunHandler {
    client: Client,
    response: Arc<JobResponse>,
    log_offset: usize,
}

impl RunHandler {
    fn new(client: Client, response: JobResponse) -> Self {
        let response = Arc::new(response);
        Self {
            client,
            response,
            log_offset: 0,
        }
    }

    async fn update(&self, state: JobState) -> Result<(), crate::client::Error> {
        self.client
            .update_job(self.response.id, &self.response.token, state)
            .await
    }

    // TODO rename this to trace
    async fn log(&mut self, data: Bytes) -> Result<(), crate::client::Error> {
        let len = data.len();

        self.client
            .trace(
                self.response.id,
                &self.response.token,
                data,
                self.log_offset,
                len,
            )
            .await?;
        self.log_offset += len;

        Ok(())
    }

    async fn run<F, J, Ret>(&mut self, process: F)
    where
        F: Fn(Job) -> Ret + Send + Sync + 'static,
        J: JobHandler + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel(16);

        let job = Job::new(self.client.clone(), self.response.clone(), tx);
        let join = tokio::spawn(run(
            job,
            self.client.clone(),
            self.response.clone(),
            process,
        ));
        tokio::pin!(join);

        loop {
            // TODO send keepalives to the server and monitor for cancellatiion
            tokio::select! {
                r = rx.recv()  => {
                    match r {
                        Some(JobRequest::Trace(b)) => self.log(b).await.unwrap(),
                        None => break,
                    }
                },
            }
        }
        let state = match join.await {
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
        F: Fn(Job) -> Ret + Sync + Send + 'static,
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
