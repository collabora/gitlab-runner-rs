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
//!         "runner system id".to_owned(),
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
use job::{Job, JobLog};
pub mod uploader;
pub use logging::GitlabLayer;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
pub use uploader::UploadFile;
mod runlist;
use crate::runlist::RunList;

use futures::prelude::*;
use futures::AsyncRead;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing::warn;
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

/// A file that a [`JobHandler`] is willing to upload to the server
///
/// A [`JobHandler`] will be queried for the set of files it is able
/// to upload to the server. These might be on disk, or they might be
/// generated or downloaded from some other source on demand. The
/// `get_path` method is required so that the globbing Gitlab expects
/// can be performed without the handler needing to be involved.
pub trait UploadableFile: Eq {
    /// The type of the data stream returned by
    /// [`get_data`](Self::get_data)
    type Data<'a>: AsyncRead + Send + Unpin
    where
        Self: 'a;

    /// Get the logical path of the file.
    ///
    /// This is the path on disk from the root of the checkout for
    /// Gitlab's own runner. It should match the paths that users are
    /// expected to specify when requesting artifacts in their jobs.
    fn get_path(&self) -> Cow<'_, str>;

    /// Get something that can provide the data for this file.
    ///
    /// This can be any implementor of [`AsyncRead`].
    fn get_data(&self) -> Self::Data<'_>;
}

/// An [`UploadableFile`] type for JobHandlers that expose no files.
///
/// This will panic on attempting to actually pass it to the crate,
/// but if you don't want or need to return artifacts to Gitlab it
/// provides a sensible default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoFiles {}

impl UploadableFile for NoFiles {
    type Data<'a> = &'a [u8];

    fn get_path(&self) -> Cow<'_, str> {
        unreachable!("tried to get path of NoFiles")
    }
    fn get_data(&self) -> Self::Data<'_> {
        unreachable!("tried to read data from NoFiles");
    }
}

/// Async trait for handling a single Job that handles its own cancellation
///
/// This trait is largely identical to [`JobHandler`], but its methods explicitly take a
/// `CancellationToken`, which will be triggered if GitLab cancels the job, after which the method
/// will be responsible for cancellation appropriately. In most cases, the entire execution should
/// simply be cancelled, in which case [`JobHandler`]'s default behavior is desirable instead. (Even
/// when cancelled, `cleanup` will still be called, allowing any cleanup tasks to be performed.)
///
/// Note that this is an asynchronous trait which should be implemented by using the [`async_trait`]
/// crate. However this also means the rustdoc documentation is interesting...
#[async_trait::async_trait]
pub trait CancellableJobHandler<U = NoFiles>: Send
where
    U: UploadableFile + Send + 'static,
{
    /// Do a single step of a job
    ///
    /// This gets called for each phase of the job (e.g. script and after_script). The passed
    /// string array is the same array as was passed for a given step in the job definition. If the
    /// job is cancelled while this is running, the given `cancel_token` will be triggered.
    ///
    /// Note that gitlab concatinates the `before_script` and `script` arrays into a single
    /// [Phase::Script] step
    async fn step(
        &mut self,
        script: &[String],
        phase: Phase,
        cancel_token: &CancellationToken,
    ) -> JobResult;

    /// Get a list of the files available to upload
    ///
    /// See the description [`UploadableFile`] for more information.
    async fn get_uploadable_files(&mut self) -> Result<Box<dyn Iterator<Item = U> + Send>, ()> {
        Ok(Box::new(core::iter::empty()))
    }

    /// Cleanup after the job is finished
    ///
    /// This method always get called whether or not the job succeeded or was acancelled, allowing
    /// the job handler to clean up as necessary.
    async fn cleanup(&mut self) {}
}

/// Async trait for handling a single Job
///
/// In the event of being cancelled by GitLab, the `step` function will have its future dropped
/// instantly. If manual handling of cancellation is required, use `CancellableJobHandler` instead.
/// (Even when cancelled, `cleanup` will still be called, allowing any cleanup tasks to be
/// performed.)
///
/// Note that this is an asynchronous trait which should be implemented by using the [`async_trait`]
/// crate. However this also means the rustdoc documentation is interesting...
#[async_trait::async_trait]
pub trait JobHandler<U = NoFiles>: Send
where
    U: UploadableFile + Send + 'static,
{
    /// Do a single step of a job
    ///
    /// This gets called for each phase of the job (e.g. script and after_script). The passed
    /// string array is the same array as was passed for a given step in the job definition. If the
    /// job is cancelled while this is running, its future will dropped, resulting in the function's
    /// termination.
    ///
    /// Note that gitlab concatinates the `before_script` and `script` arrays into a single
    /// [Phase::Script] step
    async fn step(&mut self, script: &[String], phase: Phase) -> JobResult;

    /// Get a list of the files available to upload
    ///
    /// See the description [`UploadableFile`] for more information.
    async fn get_uploadable_files(&mut self) -> Result<Box<dyn Iterator<Item = U> + Send>, ()> {
        Ok(Box::new(core::iter::empty()))
    }

    /// Cleanup after the job is finished
    ///
    /// This method always get called whether or not the job succeeded or was cancelled, allowing
    /// the job handler to clean up as necessary.
    async fn cleanup(&mut self) {}
}

#[async_trait::async_trait]
impl<J, U> CancellableJobHandler<U> for J
where
    J: JobHandler<U>,
    U: UploadableFile + Send + 'static,
{
    async fn step(
        &mut self,
        script: &[String],
        phase: Phase,
        cancel_token: &CancellationToken,
    ) -> JobResult {
        tokio::select! {
            r = self.step(script, phase) => r,
            _ = cancel_token.cancelled() => Ok(()),
        }
    }

    async fn get_uploadable_files(&mut self) -> Result<Box<dyn Iterator<Item = U> + Send>, ()> {
        self.get_uploadable_files().await
    }

    async fn cleanup(&mut self) {
        self.cleanup().await;
    }
}

/// Runner for gitlab
///
/// The runner is responsible for communicating with gitlab to request new job and spawn them.
#[derive(Debug)]
pub struct Runner {
    client: Client,
    build_dir: PathBuf,
    run_list: RunList<u64, JobLog>,
}

impl Runner {
    /// Create a new Runner for the given server url, runner token and system ID, storing
    /// (temporary job files) in build_dir
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
    ///     "RunnerSystemID".to_string(),
    ///     dir.path().to_path_buf());
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if a default subscriber is already setup
    pub fn new(server: Url, token: String, system_id: String, build_dir: PathBuf) -> Self {
        let (runner, layer) = Self::new_with_layer(server, token, system_id, build_dir);
        Registry::default().with(layer).init();

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
    ///     "RunnerSystemID".to_string(),
    ///     dir.path().to_path_buf());
    /// let subscriber = Registry::default().with(layer).init();
    /// ```
    pub fn new_with_layer(
        server: Url,
        token: String,
        system_id: String,
        build_dir: PathBuf,
    ) -> (Self, GitlabLayer) {
        let client = Client::new(server, token, system_id);
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
    pub async fn request_job<F, J, U, Ret>(&mut self, process: F) -> Result<bool, client::Error>
    where
        F: FnOnce(Job) -> Ret + Sync + Send + 'static,
        J: CancellableJobHandler<U> + Send + 'static,
        U: UploadableFile + Send + 'static,
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
    pub async fn run<F, U, J, Ret>(
        &mut self,
        process: F,
        maximum: usize,
    ) -> Result<(), client::Error>
    where
        F: Fn(Job) -> Ret + Sync + Send + 'static + Clone,
        J: CancellableJobHandler<U> + Send + 'static,
        U: UploadableFile + Send + 'static,
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
