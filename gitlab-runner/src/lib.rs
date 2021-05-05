mod client;

use crate::client::Client;
mod run;
use crate::run::Run;

pub mod job;
use job::Job;

pub mod uploader;
use log::warn;
use uploader::Uploader;

mod artifact;

use futures::prelude::*;
use futures::stream::FuturesUnordered;
use tokio::time::{sleep, Duration};
use url::Url;

pub type JobResult = Result<(), ()>;
pub use client::Phase;

#[async_trait::async_trait]
pub trait JobHandler: Send {
    /// normal job; then after depending on successfulnness
    async fn step(&mut self, script: &[String], phase: Phase) -> JobResult;
    /// upload artifacts if requested; provide paths and stuff?
    async fn upload_artifacts(&mut self, _uploader: &mut Uploader) -> JobResult {
        Ok(())
    }
    /// cleanup always called regardless of state
    async fn cleanup(&mut self) {}
}

#[derive(Debug)]
pub struct Runner {
    client: Client,
    running: FuturesUnordered<Run>,
}

impl Runner {
    pub fn new(server: Url, token: String) -> Self {
        let client = Client::new(server, token);
        let running = FuturesUnordered::new();
        Self { client, running }
    }

    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Request a new job from gitlab
    pub async fn request_job<F, J, Ret>(&mut self, process: F) -> Result<bool, client::Error>
    where
        F: Fn(Job) -> Ret + Sync + Send + 'static,
        J: JobHandler + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let response = self.client.request_job().await?;
        if let Some(response) = response {
            self.running
                .push(Run::new(process, self.client.clone(), response));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Wait for at least one of the current jobs to finish
    pub async fn wait_job(&mut self) {
        self.running.next().await;
    }

    /// Sleep for the given duration while keeping track of outstanding jobs
    pub async fn sleep(&mut self, timeout: Duration) {
        let t = sleep(timeout).fuse();
        if self.running.is_empty() {
            t.await;
        } else {
            futures::pin_mut!(t);
            loop {
                futures::select! {
                    _ = t => break,
                    _ = self.running.next() => {},
                };
            }
        }
    }

    pub async fn run<F, J, Ret>(&mut self, process: F, maximum: usize) -> Result<(), client::Error>
    where
        F: Fn(Job) -> Ret + Sync + Send + 'static + Clone,
        J: JobHandler + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        loop {
            if self.running.len() < maximum {
                match self.request_job(process.clone()).await {
                    /* continue to pick up a new job straight away */
                    Ok(true) => continue,
                    Ok(false) => (),
                    Err(e) => warn!("Couldn't get a job from gitlab: {:?}", e),
                }
                self.sleep(Duration::from_secs(5)).await;
            } else {
                self.wait_job().await;
            }
        }
    }
}
