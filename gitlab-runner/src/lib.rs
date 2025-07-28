#![warn(missing_docs)]
#![doc = include_str!("../README.md")]
pub mod artifact;
mod client;
mod logging;
use crate::client::Client;
mod run;
use crate::run::Run;
pub mod job;
use client::{ClientMetadata, GitCheckoutError};
use gix::{clone, create, index, open, progress, refspec, remote, DynNestedProgress};
use hmac::Hmac;
use hmac::Mac;
use job::{Job, JobLog};
pub mod uploader;
pub use logging::GitlabLayer;
use rand::distr::Alphanumeric;
use rand::distr::SampleString;
use runlist::JobRunList;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;
use tracing::{debug, trace, warn};

mod runlist;
use crate::runlist::RunList;

use futures::prelude::*;
use futures::AsyncRead;
use std::borrow::Cow;
use std::fmt::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
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
#[async_trait::async_trait]
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
    async fn get_data(&self) -> Result<Self::Data<'_>, ()>;
}

/// An [`UploadableFile`] type for JobHandlers that expose no files.
///
/// This will panic on attempting to actually pass it to the crate,
/// but if you don't want or need to return artifacts to Gitlab it
/// provides a sensible default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoFiles {}

#[async_trait::async_trait]
impl UploadableFile for NoFiles {
    type Data<'a> = &'a [u8];

    fn get_path(&self) -> Cow<'_, str> {
        unreachable!("tried to get path of NoFiles")
    }
    async fn get_data(&self) -> Result<Self::Data<'_>, ()> {
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
            Alphanumeric.append_string(&mut rand::rng(), &mut system_id, Self::DEFAULT_ID_LEN);
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

// TODO: Should we re-export gix::progress to provide a means to monitor progress
// TODO: Is "clippy::result_large_err" the best solution to GitCheckoutError

/// Fetch and checkout a given worktree on a thread
///
/// See: [clone_git_repository_sync]
pub async fn clone_git_repository(
    parent_path: &Path,
    repo_url: &str,
    head_ref: Option<&str>,
    refspecs: impl IntoIterator<Item = impl AsRef<str>>,
    depth: Option<u32>,
    cancel_token: CancellationToken,
) -> Result<PathBuf, GitCheckoutError> {
    let parent_path = parent_path.to_owned();
    let repo_url = repo_url.to_owned();
    let head_ref = head_ref.map(|a| a.to_owned());
    let should_interrupt: Arc<AtomicBool> = Default::default();
    let should_interrupt_cancel = should_interrupt.clone();
    let refspecs: Vec<_> = refspecs
        .into_iter()
        .map(|s| s.as_ref().to_owned())
        .collect();
    // offload the clone operation to one of the tokio runtimes blocking threads
    tokio::select! {
        result = tokio::task::spawn_blocking(move || {
            clone_git_repository_sync(
                &parent_path,
                &repo_url,
                head_ref.as_deref(),
                refspecs,
                depth,
                &should_interrupt,
            )
        }) => result?,
        _ = cancel_token.cancelled() => {
            should_interrupt_cancel.store(true, Ordering::SeqCst);
            Err(GitCheckoutError::Cancelled)
        }
    }
}

/// Fetch and checkout a given worktree
///
/// This creates a new path for the repo as gitoxide deletes it on failure.
#[allow(clippy::result_large_err)]
pub fn clone_git_repository_sync(
    parent_path: &Path,
    repo_url: &str,
    head_ref: Option<&str>,
    refspecs: impl IntoIterator<Item = impl AsRef<str>>,
    depth: Option<u32>,
    should_interrupt: &AtomicBool,
) -> Result<PathBuf, GitCheckoutError> {
    let repo_dir = tempfile::Builder::new()
        .prefix("repo_")
        .tempdir_in(parent_path)?;

    // TODO: Should we expose the ability to interrupt / report progress
    let mut progress = progress::Discard;

    // TODO: Is Options::isolated correct here?
    //  The url provided from gitlab has the credentials
    let mut fetch = clone::PrepareFetch::new(
        repo_url,
        repo_dir.path(),
        create::Kind::WithWorktree,
        Default::default(),
        open::Options::isolated(),
    )?;

    // Specify which refspecs are fetched from remote
    let mut refspecs = refspecs
        .into_iter()
        .map(|s| {
            refspec::parse(s.as_ref().into(), refspec::parse::Operation::Fetch)
                .map(|r| r.to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut sha = None;

    if let Some(head_ref) = head_ref {
        if let Ok(object_id) = gix::ObjectId::from_hex(head_ref.as_bytes()) {
            // TODO: I need to specify that the sha should be fetched from remote, right?
            //  Is there a format for a sha as a refspec?
            if let Ok(refspec) = refspec::parse(
                format!("+{head_ref}").as_str().into(),
                refspec::parse::Operation::Fetch,
            ) {
                refspecs.push(refspec.to_owned());
            }
            sha = Some(object_id);
        } else {
            // TODO: For some reason this doesn't create refs/heads/{reference}
            if let Ok(refspec) = refspec::parse(
                format!("+refs/heads/{head_ref}:refs/remotes/origin/{head_ref}")
                    .as_str()
                    .into(),
                refspec::parse::Operation::Fetch,
            ) {
                refspecs.push(refspec.to_owned());
            }
        }
    }

    if !refspecs.is_empty() {
        let mut fetch_opts = remote::ref_map::Options::default();
        fetch_opts.extra_refspecs.extend(refspecs);
        fetch = fetch.with_fetch_options(fetch_opts);
    }

    if let Some(depth) = depth.and_then(NonZeroU32::new) {
        fetch = fetch.with_shallow(remote::fetch::Shallow::DepthAtRemote(depth));
    }

    let checkout_progress = progress.add_child("checkout".to_string());
    let (checkout, outcome) = fetch.fetch_then_checkout(checkout_progress, should_interrupt)?;

    trace!("git clone fetch outcome for {repo_url}: {outcome:?}");

    let repo = checkout.persist();

    let root_tree_id = if let Some(sha) = sha {
        // Checkout worktree at specific SHA
        repo.try_find_object(sha)?
            .ok_or(GitCheckoutError::MissingCommit)?
    } else if let Some(reference) = head_ref {
        repo
            // TODO: This should be refs/heads/{reference} but it doesn't exist
            .try_find_reference(format!("refs/remotes/origin/{reference}").as_str())?
            .ok_or(GitCheckoutError::MissingCommit)?
            .peel_to_id_in_place()?
            .object()?
    } else {
        // Checkout head
        repo.head()?
            .try_peel_to_id_in_place()?
            .ok_or(GitCheckoutError::MissingCommit)?
            .object()?
    };
    let root_tree = root_tree_id.peel_to_tree()?.id;

    let index =
        index::State::from_tree(&root_tree, &repo.objects, Default::default()).map_err(|err| {
            clone::checkout::main_worktree::Error::IndexFromTree {
                id: root_tree,
                source: err,
            }
        })?;

    let workdir =
        repo.work_dir()
            .ok_or_else(|| clone::checkout::main_worktree::Error::BareRepository {
                git_dir: repo.git_dir().to_owned(),
            })?;

    let files_progress = progress.add_child_with_id(
        "files".to_string(),
        clone::checkout::main_worktree::ProgressId::CheckoutFiles.into(),
    );
    let bytes_progress = progress.add_child_with_id(
        "bytes".to_string(),
        clone::checkout::main_worktree::ProgressId::BytesWritten.into(),
    );
    let mut index = index::File::from_state(index, repo.index_path());
    let outcome = gix::worktree::state::checkout(
        &mut index,
        workdir,
        repo.objects.clone().into_arc()?,
        &files_progress,
        &bytes_progress,
        should_interrupt,
        Default::default(),
    )?;

    trace!("git clone checkout outcome for {repo_url}: {outcome:?}");

    index.write(Default::default())?;

    Ok(repo_dir.keep())
}
