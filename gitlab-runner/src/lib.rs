#![warn(missing_docs)]
#![doc = include_str!("../README.md")]
pub mod artifact;
mod client;
mod logging;
use crate::client::Client;
mod run;
use crate::run::Run;
pub mod job;
use client::ClientMetadata;
use hmac::Hmac;
use hmac::Mac;
use job::{Job, JobLog};
pub mod uploader;
pub use logging::GitlabLayer;
use rand::distributions::Alphanumeric;
use rand::distributions::DistString;
use runlist::JobRunList;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::instrument::WithSubscriber;

mod runlist;
use crate::runlist::RunList;

use futures::prelude::*;
use futures::AsyncRead;
use std::borrow::Cow;
use std::fmt::Write;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing::warn;
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

/// Builder for [`Runner`]
pub struct RunnerBuilder {
    server: Url,
    token: String,
    build_dir: PathBuf,
    system_id: Option<String>,
    run_list: RunList<u64, JobLog>,
    metadata: ClientMetadata,
}

impl RunnerBuilder {
    // The official gitlab runner uses 2 char prefixes (s_ or r_) followed
    // by 12 characters for a unique identifier
    const DEFAULT_ID_LEN: usize = 12;
    /// Create a new [`RunnerBuilder`] for the given server url, runner token,
    /// build dir and job list (as created by GitlabLayer::new).
    ///
    /// The build_dir is used to store temporary files during a job run.
    /// ```
    /// # use tracing_subscriber::{prelude::*, Registry};
    /// # use gitlab_runner::{RunnerBuilder, GitlabLayer};
    /// # use url::Url;
    /// #
    /// #[tokio::main]
    /// # async fn main() {
    /// let dir = tempfile::tempdir().unwrap();
    /// let (layer, jobs) = GitlabLayer::new();
    /// let subscriber = Registry::default().with(layer).init();
    /// let runner = RunnerBuilder::new(
    ///         Url::parse("https://gitlab.com/").unwrap(),
    ///         "RunnerToken",
    ///         dir.path(),
    ///         jobs
    ///     )
    ///     .build()
    ///     .await;
    /// # }
    /// ```
    pub fn new<P: Into<PathBuf>, S: Into<String>>(
        server: Url,
        token: S,
        build_dir: P,
        jobs: JobRunList,
    ) -> Self {
        RunnerBuilder {
            server,
            token: token.into(),
            build_dir: build_dir.into(),
            system_id: None,
            run_list: jobs.inner(),
            metadata: ClientMetadata::default(),
        }
    }

    /// Set the [system_id](https://docs.gitlab.com/runner/fleet_scaling/#generation-of-system_id-identifiers) for this runner
    ///
    /// The system_id will be truncated to 64 characters to match gitlabs limit,
    /// but no further validation will be done. It's up to the caller to ensure the
    /// system_id is valid for gitlab
    pub fn system_id<S: Into<String>>(mut self, system_id: S) -> Self {
        let mut system_id = system_id.into();
        system_id.truncate(64);
        self.system_id = Some(system_id);
        self
    }

    /// Set the version reported by the gitlab runner
    ///
    /// The version will be truncated to 2048 characters
    pub fn version<S: Into<String>>(mut self, version: S) -> Self {
        let mut version = version.into();
        version.truncate(2048);
        self.metadata.version = Some(version);
        self
    }

    /// Set the revision reported by the gitlab runner
    ///
    /// The revision will be truncated to 255 characters
    pub fn revision<S: Into<String>>(mut self, revision: S) -> Self {
        let mut revision = revision.into();
        revision.truncate(255);
        self.metadata.revision = Some(revision);
        self
    }

    /// Set the platform reported by the gitlab runner
    ///
    /// The platform will be truncated to 255 characters
    pub fn platform<S: Into<String>>(mut self, platform: S) -> Self {
        let mut platform = platform.into();
        platform.truncate(255);
        self.metadata.platform = Some(platform);
        self
    }

    /// Set the architecture reported by the gitlab runner
    ///
    /// The architecture will be truncated to 255 characters
    pub fn architecture<S: Into<String>>(mut self, architecture: S) -> Self {
        let mut architecture = architecture.into();
        architecture.truncate(255);
        self.metadata.architecture = Some(architecture);
        self
    }

    async fn generate_system_id_from_machine_id() -> Option<String> {
        let mut f = match File::open("/etc/machine-id").await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("/etc/machine-id not found, not generate systemd id based on it");
                return None;
            }
            Err(e) => {
                warn!("Failed to open machine-id: {e}");
                return None;
            }
        };

        let mut id = [0u8; 32];
        if let Err(e) = f.read_exact(&mut id).await {
            warn!("Failed to read from machine-id: {e}");
            return None;
        };

        // Infallible as a hmac can take a key of any size
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&id).unwrap();
        mac.update(b"gitlab-runner");

        let mut system_id = String::from("s_");
        // 2 hex chars for each byte
        for b in &mac.finalize().into_bytes()[0..Self::DEFAULT_ID_LEN / 2] {
            // Infallible: writing to a string
            write!(&mut system_id, "{:02x}", b).unwrap();
        }
        Some(system_id)
    }

    async fn generate_system_id() -> String {
        if let Some(system_id) = Self::generate_system_id_from_machine_id().await {
            system_id
        } else {
            let mut system_id = String::from("r_");
            Alphanumeric.append_string(
                &mut rand::thread_rng(),
                &mut system_id,
                Self::DEFAULT_ID_LEN,
            );
            system_id
        }
    }

    /// Build the runner.
    pub async fn build(self) -> Runner {
        let system_id = match self.system_id {
            Some(system_id) => system_id,
            None => Self::generate_system_id().await,
        };
        let client = Client::new(self.server, self.token, system_id, self.metadata);
        Runner {
            client,
            build_dir: self.build_dir,
            run_list: self.run_list,
        }
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
    /// Note that this function is not cancel safe. If the future gets cancelled gitlab might have
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
