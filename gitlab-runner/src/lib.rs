pub mod artifact;
mod client;
mod logging;
use crate::client::Client;
mod run;
use crate::run::Run;
pub mod job;
use job::{Job, JobData};
pub mod uploader;
pub use logging::GitlabLayer;
use tracing::instrument::WithSubscriber;
mod runlist;
use crate::runlist::RunList;
use uploader::Uploader;

use futures::prelude::*;
use log::warn;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing_subscriber::{prelude::*, Registry};

use url::Url;

#[cfg(doctest)]
doc_comment::doctest!("../../README.md");

pub type JobResult = Result<(), ()>;
pub use client::Phase;

#[doc(hidden)]
pub use ::tracing;
#[macro_export]
macro_rules! outputln {
      ($f: expr) => {
          $crate::tracing::info!(gitlab.output = true, $f)
      };
      ($f: expr, $($arg: tt) *) => {
          $crate::tracing::trace!(gitlab.output = true, $f, $($arg)*)
      };
  }

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
    /// Create a new Runner for the given server url and runner token, storing (temporary job
    /// files) in build_dir
    ///
    /// The build_dir is used to store temporary files during a job run. This will also configure a
    /// default tracing subscriber if that's not wanted use [`Runner::new_with_layer`] instead.
    ///
    /// ```
    /// # use gitlab_runner::Runner;
    /// # use url::Url;
    /// #
    /// let dir = tempfile::tempdir().unwrap();
    /// let runner = Runner::new(Url::parse("https://gitlab.com/").unwrap(),
    ///     "RunnerToken".to_string(),
    ///     dir.path().to_path_buf());
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a default subscriber is already setup
    pub fn new(server: Url, token: String, build_dir: PathBuf) -> Self {
        let (runner, layer) = Self::new_with_layer(server, token, build_dir);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::set_global_default(subscriber)
            .expect("Couldn't set global default subscriber (already set?)");

        runner
    }

    /// Creates a new runner as per [`Runner::new`] and logging layer
    ///
    /// The logging layer should attached to the current tracing subscriber while further runner
    /// calls are made otherwise job logging to gitlab will not work
    ///
    /// ```
    /// # use gitlab_runner::Runner;
    /// # use url::Url;
    /// # use tracing_subscriber::{prelude::*, Registry};
    /// #
    /// let dir = tempfile::tempdir().unwrap();
    /// let (runner, layer) = Runner::new_with_layer(Url::parse("https://gitlab.com/").unwrap(),
    ///     "RunnerToken".to_string(),
    ///     dir.path().to_path_buf());
    /// let subscriber = Registry::default().with(layer);
    ///
    /// tracing::subscriber::set_global_default(subscriber)
    ///     .expect("Couldn't set global default subscriber (already set?)");
    /// ```
    pub fn new_with_layer(server: Url, token: String, build_dir: PathBuf) -> (Self, GitlabLayer) {
        let client = Client::new(server, token);
        let run_list = RunList::new();
        (
            Self {
                client,
                build_dir,
                run_list: run_list.clone(),
            },
            GitlabLayer::new(run_list),
        )
    }

    /// Returns te number of jobs currently running
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
            tokio::spawn(
                async move { run.run(process, build_dir).await }.with_current_subscriber(),
            );
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
