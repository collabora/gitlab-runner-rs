mod client;

use std::path::PathBuf;

use crate::client::Client;
mod run;
use crate::run::Run;

pub mod job;
use job::{Job, JobData};

pub mod uploader;
use runlist::RunList;
use uploader::Uploader;

mod artifact;
mod runlist;

use futures::prelude::*;
use log::warn;
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
    build_dir: PathBuf,
    run_list: RunList<u64, JobData>,
}

impl Runner {
    pub fn new(server: Url, token: String, build_dir: PathBuf) -> Self {
        let client = Client::new(server, token);
        let run_list = RunList::new();
        Self {
            client,
            build_dir,
            run_list,
        }
    }

    pub fn running(&self) -> usize {
        self.run_list.size()
    }

    /// Request a new job from gitlab
    pub async fn request_job<F, J, Ret>(&mut self, process: F) -> Result<bool, client::Error>
    where
        F: FnOnce(Job) -> Ret + Sync + Send + 'static,
        J: JobHandler + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        let response = self.client.request_job().await?;
        if let Some(response) = response {
            let mut build_dir = self.build_dir.clone();
            build_dir.push(format!("{}", response.id));
            let mut run = Run::new(self.client.clone(), response, &mut self.run_list);
            tokio::spawn(async move { run.run(process, build_dir).await });
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Wait untill there are leass then max jobs running
    pub async fn wait_for_space(&mut self, max: usize) {
        self.run_list.wait_for_space(max).await;
    }

    pub async fn run<F, J, Ret>(&mut self, process: F, maximum: usize) -> Result<(), client::Error>
    where
        F: Fn(Job) -> Ret + Sync + Send + 'static + Clone,
        J: JobHandler + Send + 'static,
        Ret: Future<Output = Result<J, ()>> + Send + 'static,
    {
        loop {
            self.wait_for_space(maximum).await;
            match self.request_job(process.clone()).await {
                /* continue to pick up a new job straight away */
                Ok(true) => continue,
                Ok(false) => (),
                Err(e) => warn!("Couldn't get a job from gitlab: {:?}", e),
            }
            sleep(Duration::from_secs(5)).await;
        }
    }
}
