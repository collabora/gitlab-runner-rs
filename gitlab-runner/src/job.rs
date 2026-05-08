//! This module describes a single gitlab job
use crate::artifact::Artifact;
use crate::client::{
    Client, GitCheckoutError, GitStrategy, JobArtifactFile, JobDependency, JobResponse, JobVariable,
};
use crate::outputln;
use bytes::{Bytes, BytesMut};
use gix::NestedProgress;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::io::AsyncWrite;
use tokio_retry2::strategy::{FibonacciBackoff, jitter};
use tracing::{info, trace};

use crate::client::Error as ClientError;

/// Gitlab job environment variable
///
/// To get the underlying value [`Variable::value`] should be used, however this should not be
/// directly displayed in the log. For displaying to the user the Variable's
/// [`Display`](`std::fmt::Display`)
/// implementation should be used (e.g. via `{}` as that will show the value taking the masked
/// status into account
pub struct Variable<'a> {
    v: &'a JobVariable,
}

impl<'a> Variable<'a> {
    /// Get the key of the variable
    pub fn key(&self) -> &'a str {
        &self.v.key
    }

    /// Get the value of the variable
    pub fn value(&self) -> &'a str {
        &self.v.value
    }

    /// Whether or not the variable is masked
    pub fn masked(&self) -> bool {
        self.v.masked
    }

    /// Whether or not the variable is public
    pub fn public(&self) -> bool {
        self.v.public
    }
}

impl std::fmt::Display for Variable<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        if self.v.masked {
            write!(f, "<MASKED>")
        } else {
            write!(f, "{}", self.v.value)
        }
    }
}

/// A dependency of a gitlab job
///
/// Dependencies in gitlab are the jobs that had to be run before this job could run.
/// This can happen either implicitly in the pipeline definition via stages of directly
/// via `needs` keyword
#[derive(Debug)]
pub struct Dependency<'a> {
    job: &'a Job,
    dependency: &'a JobDependency,
}

fn is_retriable_error(err: &ClientError) -> bool {
    match err {
        ClientError::Request(_) => true,
        ClientError::UnexpectedStatus(status) => status.is_server_error(),
        _ => false,
    }
}

impl Dependency<'_> {
    /// The id of the dependency
    ///
    /// This id matches the job id of the generated this depenency
    pub fn id(&self) -> u64 {
        self.dependency.id
    }

    /// The name job that creaof the dependency
    ///
    /// This name matches the job name of the job that generated this depenency
    pub fn name(&self) -> &str {
        &self.dependency.name
    }

    /// The filename of the dependencies artifact if it has one
    pub fn artifact_filename(&self) -> Option<&str> {
        self.dependency
            .artifacts_file
            .as_ref()
            .map(|a| a.filename.as_str())
    }

    /// The size of the dependencies artifact if it has one
    pub fn artifact_size(&self) -> Option<usize> {
        self.dependency.artifacts_file.as_ref().map(|a| a.size)
    }

    async fn download_impl<A>(&self, mut writer: A) -> Result<(), ClientError>
    where
        A: AsyncWrite + Unpin,
    {
        let mut strategy = FibonacciBackoff::from_millis(900).map(jitter).take(4);
        loop {
            let r = self
                .job
                .client
                .download_artifact(self.dependency.id, &self.dependency.token, &mut writer)
                .await;

            match (r, strategy.next()) {
                (Err(err), Some(d)) if is_retriable_error(&err) => {
                    outputln!(
                        "Error getting artifacts from {}: {err}. Retrying",
                        self.dependency.name
                    );
                    tokio::time::sleep(d).await;
                }
                (r, _) => return r,
            }
        }
    }

    async fn download_to_file(&self, _file: &JobArtifactFile) -> Result<(), ClientError> {
        let mut path = self.job.build_dir.join("artifacts");
        if let Err(e) = tokio::fs::create_dir(&path).await
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(ClientError::WriteFailure(e));
        }

        // TODO this assumes it's all zip artifacts
        path.push(format!("{}.zip", self.id()));
        let mut f = tokio::fs::File::create(&path)
            .await
            .map_err(ClientError::WriteFailure)?;
        self.download_impl(&mut f).await?;
        self.job.artifacts.insert_file(self.dependency.id, path);
        Ok(())
    }

    async fn download_to_mem(&self, file: &JobArtifactFile) -> Result<(), ClientError> {
        let mut bytes = Vec::with_capacity(file.size);
        self.download_impl(&mut bytes).await?;
        self.job.artifacts.insert_data(self.dependency.id, bytes);
        Ok(())
    }

    /// Download dependencies artifact
    ///
    /// This downloads the actual artifact file from gitlab if it hadn't been downloaded yet.
    /// Bigger files get saved on the filesystem while small ones are simply cached in memory
    pub async fn download(&self) -> Result<Option<Artifact>, ClientError> {
        if let Some(file) = &self.dependency.artifacts_file {
            let cached = self.job.artifacts.get(self.dependency.id).await?;
            if cached.is_some() {
                return Ok(cached);
            }

            // Load up to 64 kilobyte directly into memory; bigger files to storage to not bloat
            // the memory usage
            if file.size > 64 * 1024 {
                info!("Downloading dependency {} to file", self.dependency.id);
                self.download_to_file(file).await?
            } else {
                info!("Downloading dependency {} to mem", self.dependency.id);
                self.download_to_mem(file).await?
            }
            self.job.artifacts.get(self.dependency.id).await
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
struct ArcU8(Arc<Vec<u8>>);

impl ArcU8 {
    fn new(data: Vec<u8>) -> Self {
        Self(Arc::new(data))
    }
}

impl AsRef<[u8]> for ArcU8 {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().as_ref()
    }
}

#[derive(Clone, Debug)]
enum CacheData {
    MemoryBacked(ArcU8),
    FileBacked(PathBuf),
}

#[derive(Debug)]
struct ArtifactCache {
    data: Mutex<HashMap<u64, CacheData>>,
}

impl ArtifactCache {
    fn new() -> Self {
        ArtifactCache {
            data: Mutex::new(HashMap::new()),
        }
    }

    fn insert_file(&self, id: u64, path: PathBuf) {
        let mut d = self.data.lock().unwrap();
        d.insert(id, CacheData::FileBacked(path));
    }

    fn insert_data(&self, id: u64, data: Vec<u8>) {
        let mut d = self.data.lock().unwrap();
        d.insert(id, CacheData::MemoryBacked(ArcU8::new(data)));
    }

    fn lookup(&self, id: u64) -> Option<CacheData> {
        let d = self.data.lock().unwrap();
        d.get(&id).cloned()
    }

    async fn get(&self, id: u64) -> Result<Option<Artifact>, ClientError> {
        if let Some(data) = self.lookup(id) {
            match data {
                CacheData::MemoryBacked(m) => {
                    Ok(Some(Artifact::new(Box::new(std::io::Cursor::new(m)))?))
                }
                CacheData::FileBacked(p) => {
                    let f = tokio::fs::File::open(p)
                        .await
                        .map_err(ClientError::WriteFailure)?;
                    // Always succeeds as no operations have been started
                    Ok(Some(Artifact::new(Box::new(f.try_into_std().unwrap()))?))
                }
            }
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobLog {
    trace: Arc<Mutex<BytesMut>>,
}

impl JobLog {
    pub(crate) fn new() -> Self {
        let trace = Arc::new(Mutex::new(BytesMut::new()));
        Self { trace }
    }

    pub(crate) fn trace(&self, data: &[u8]) {
        let mut trace = self.trace.lock().unwrap();
        trace.extend_from_slice(data);
    }

    pub(crate) fn split_trace(&self) -> Option<Bytes> {
        let mut trace = self.trace.lock().unwrap();
        if trace.is_empty() {
            None
        } else {
            Some(trace.split().freeze())
        }
    }
}

static HEAD: LazyLock<gix::refs::FullName> = LazyLock::new(|| "HEAD".try_into().unwrap());

fn clone_git_repository_sync(
    build_dir: PathBuf,
    repo_url: &str,
    sha: gix::ObjectId,
    refspecs: Vec<gix::refspec::RefSpec>,
    depth: u32,
    should_interrupt: &AtomicBool,
) -> Result<PathBuf, GitCheckoutError> {
    let repo_dir = tempfile::TempDir::with_prefix_in("repo_", &build_dir)?;

    // TODO: Should we expose the ability to interrupt / report progress
    let mut progress = gix::progress::Discard;

    let mut fetch_opts = gix::remote::ref_map::Options::default();
    fetch_opts.extra_refspecs.extend(refspecs);

    let mut fetch = gix::clone::PrepareFetch::new(
        repo_url,
        repo_dir.path(),
        gix::create::Kind::WithWorktree,
        Default::default(),
        gix::open::Options::isolated(),
    )?
    .with_fetch_options(fetch_opts)
    .with_shallow(if let Some(depth) = NonZeroU32::new(depth) {
        gix::remote::fetch::Shallow::DepthAtRemote(depth)
    } else {
        gix::remote::fetch::Shallow::NoChange
    });

    let fetch_progress = progress.add_child("fetch".to_string());
    let (mut checkout, outcome) = fetch.fetch_then_checkout(fetch_progress, should_interrupt)?;
    trace!("git clone fetch outcome for {repo_url}: {outcome:?}");

    checkout.repo().reference(
        HEAD.clone(),
        sha,
        gix::refs::transaction::PreviousValue::Any,
        format!("clone repo at {sha}"),
    )?;

    let (_repo, outcome) = checkout.main_worktree(progress, should_interrupt)?;
    trace!("git clone checkout outcome for {repo_url}: {outcome:?}");
    Ok(repo_dir.keep())
}

/// A running Gitlab Job
#[derive(Debug)]
pub struct Job {
    response: Arc<JobResponse>,
    client: Client,
    log: JobLog,
    build_dir: PathBuf,
    artifacts: ArtifactCache,
}

impl Job {
    pub(crate) fn new(
        client: Client,
        response: Arc<JobResponse>,
        build_dir: PathBuf,
        log: JobLog,
    ) -> Self {
        Self {
            client,
            response,
            log,
            build_dir,
            artifacts: ArtifactCache::new(),
        }
    }

    /// Get the job id
    pub fn id(&self) -> u64 {
        self.response.id
    }

    /// Sent data to the gitlab log
    ///
    /// Normally [`outputln!`](crate::outputln) should be used. This function directly puts data in the queue for
    /// the gitlab log and side-steps the tracing infrastructure
    pub fn trace<D: AsRef<[u8]>>(&self, data: D) {
        self.log.trace(data.as_ref());
    }

    /// Get the variable matching the given key
    pub fn variable(&self, key: &str) -> Option<Variable<'_>> {
        self.response.variables.get(key).map(|v| Variable { v })
    }

    /// Get an iterator over all the variables associated with this job.
    pub fn variables(&self) -> impl Iterator<Item = Variable<'_>> {
        self.response.variables.values().map(|v| Variable { v })
    }

    /// Get an iterator over the job dependencies
    pub fn dependencies(&self) -> impl Iterator<Item = Dependency<'_>> {
        self.response
            .dependencies
            .iter()
            .map(move |dependency| Dependency {
                job: self,
                dependency,
            })
    }

    /// Get a reference to the jobs build dir.
    pub fn build_dir(&self) -> &Path {
        &self.build_dir
    }

    /*
     * Notes on gitlab-runner:
     * https://gitlab.com/gitlab-org/gitlab-runner/-/blob/main/shells/abstract.go#L556
     *
     * gitlab runner:
     *  * Clones to BuildDir
     *  * Checks out git_info.sha and fetches all in git_info.refspecs
     *  * Hardcoded username "gitlab-ci-token"
     *  * 'credHelperCommand' is a shell command that's used to get the password from the env
     *  * if ".git/shallow" exists use git "--unshallow" argument
     *
     * Steps:
     *  * git config
     *  * cleanup .git directory? {index.lock, modbules, etc.}
     *  * git init
     *  * cd repo
     *  * git remote add origin
     *  * git fetch origin
     *     --no-recurse-submodules $refspecs
     *     --depth $depth $fetch_flags
     *     --unshallow if depth <= 0 && exists(.git/shallow)
     *  * git checkout $sha
     */

    /// Fetch and checkout worktree for job, returning the checkout directory
    ///
    /// If GIT_STRATEGY is set to disable fetching (i.e. `none` or `empty`),
    /// returns None.
    pub async fn clone_git_repository(&self) -> Result<Option<PathBuf>, GitCheckoutError> {
        if let Some(strategy) = self.variable("GIT_STRATEGY")
            && matches!(
                GitStrategy::from_str(strategy.value())?,
                GitStrategy::None | GitStrategy::Empty
            )
        {
            return Ok(None);
        }

        struct InterruptOnDrop {
            should_interrupt: Arc<AtomicBool>,
        }

        impl Drop for InterruptOnDrop {
            fn drop(&mut self) {
                self.should_interrupt.store(true, Ordering::Relaxed);
            }
        }

        let build_dir = self.build_dir.to_owned();
        let repo_url = self.response.git_info.repo_url.to_owned();
        let sha = gix::ObjectId::from_hex(self.response.git_info.sha.as_bytes())?;
        let refspecs = self
            .response
            .git_info
            .refspecs
            .iter()
            .map(|s| {
                gix::refspec::parse(s.as_str().into(), gix::refspec::parse::Operation::Fetch)
                    .map(|r| r.to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let depth = self.response.git_info.depth;

        let should_interrupt: Arc<AtomicBool> = Default::default();
        let _interrupt = InterruptOnDrop {
            should_interrupt: should_interrupt.clone(),
        };

        // offload the clone operation to one of the tokio runtimes blocking threads
        tokio::task::spawn_blocking(move || {
            clone_git_repository_sync(
                build_dir,
                &repo_url,
                sha,
                refspecs,
                depth,
                &should_interrupt,
            )
        })
        .await?
        .map(Some)
    }
}
