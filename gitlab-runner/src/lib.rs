#![warn(missing_docs)]
//! A crate to help build custom gitlab runner implementations.
//!
//! ## Implementing a custom runner
//!
//! The overall idea is that this crate handles all interaction with the gitlab
//! server and drives the executions while the the runner implementation focus on
//! how to handle jobs from gitlab.
//!
//! As the focus for an implementer of a custom runner is to implement the async
//! JobHandler trait, which gets calle during job executation. An absolute minimal
//! runner can be implement as such:
//!
//! ```rust,no_run
//! use gitlab_runner::{outputln, Runner, JobHandler, JobResult, Phase};
//! use std::path::PathBuf;
//!
//! #[derive(Debug)]
//! struct Run {}
//!
//! #[async_trait::async_trait]
//! impl JobHandler for Run {
//!       async fn step(&mut self, script: &[String], phase: Phase) -> JobResult {
//!           outputln!("Running script for phase {:?}", phase);
//!           for s in script {
//!             outputln!("Step: {}", s);
//!           }
//!           Ok(())
//!       }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut runner = Runner::new(
//!         "https://gitlab.example.com".try_into().unwrap(),
//!         "runner token".to_owned(),
//!         PathBuf::from("/tmp"));
//!     runner.run(move | _job | async move { Ok(Run{})  }, 16).await.unwrap();
//! }
//! ```
//!
//! ## Gitlab runner registration
//!
//! This crate does not support registering new runners with the gitlab server, so this has to be
//! done by hand using the gitlab
//! [runner registration API](https://docs.gitlab.com/ee/api/runners.html#register-a-new-runner).
//!
//! The registration token can be retrieved from the runners section in the Gitlab
//! administration area. With that token the runner can be register using a curl
//! command like:
//! ```shell
//! curl --request POST "https://GITLAB_URL/api/v4/runners"  \
//!   --form "description=My custom runner" \
//!   --form "run_untagged=false" \
//!   --form "tag_list=custom-gitlab-runner" \
//!   --form "token=REGISTRATION_TOKEN"
//! ```
//!
//! As a response to this command a new token for the registered runner will be
//! provided, this token should be provided to the runner for it's gitlab
//! connection.
//!
//! One thing to key parameter provided here is `run_untagged=false`, which will
//! make the runner *only* pickup jobs which matches its tag. This is important to
//! prevent the runner from picking up "normal" jobs which it will not be able to
//! process.
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

#[doc(hidden)]
pub use ::tracing;

/// Output a line to the gitlab log
#[macro_export]
macro_rules! outputln {
    ($f: expr) => {
        $crate::tracing::trace!(gitlab.output = true, $f)
    };
    ($f: expr, $($arg: tt) *) => {
        $crate::tracing::trace!(gitlab.output = true, $f, $($arg)*)
    };
}

/// Result type for various stagings of a jobs
pub type JobResult = Result<(), ()>;
pub use client::Phase;

/// Async trait for handling a single Job
///
/// Note that this is an asynchronous trait which should be implemented by using the [`async_trait`]
/// crate. However this also means the rustdoc documentation is interesting...
#[async_trait::async_trait]
pub trait JobHandler: Send {
    /// Do a single step of a job
    ///
    /// This gets called for each phase of the job (e.g. script and after_script). The passed
    /// string array is the same array as was passed for a given step in the job definition.cold
    ///
    /// Note that gitlab concatinates the `before_script` and `script` arrays into a single
    /// [Phase::Script] step
    async fn step(&mut self, script: &[String], phase: Phase) -> JobResult;
    /// Upload artifacts to gitlab
    ///
    /// This gets called depending on whether the job definition calls for artifacts to be uploaded
    /// based on the result of the script run
    async fn upload_artifacts(&mut self, _uploader: &mut Uploader) -> JobResult {
        Ok(())
    }
    /// Cleanup after the job is finished
    ///
    /// This method always get called whether or not the job succeeded, allowing the job handler to
    /// clean up as necessary.
    async fn cleanup(&mut self) {}
}

/// Runner for gitlab
///
/// The runner is responsible for communicating with gitlab to request new job and spawn them.
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
    /// let subscriber = Registry::default().with(layer).init();
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

    /// The number of jobs currently running
    pub fn running(&self) -> usize {
        self.run_list.size()
    }

    /// Try to request a single job from gitlab
    ///
    /// This does a single poll of gitlab for a new job. If a new job received a new asynchronous
    /// task is spawned for processing the job. The passed `process` function is called to create a
    /// the actual job handler. Returns whether or not a job was received or an error if polling
    /// gitlab failed.
    ///
    /// Note that this function is not cancel save. If the future gets cancelled gitlab might have
    /// provided a job for which processing didn't start yet.
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

    /// Wait untill there are less then max jobs running
    pub async fn wait_for_space(&mut self, max: usize) {
        self.run_list.wait_for_space(max).await;
    }

    /// Wait untill there are no more jobs running
    pub async fn drain(&mut self) {
        self.run_list.wait_for_space(1).await;
    }

    /// Run continously, processing at most `maximum` jobs concurrently
    ///
    /// This essentially calls [`Runner::request_job`] requesting jobs until at most `maximum` jobs are
    /// running in parallel.
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
