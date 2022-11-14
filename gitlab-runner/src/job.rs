//! This module describes a single gitlab job
use crate::artifact::Artifact;
use crate::client::{Client, JobArtifactFile, JobDependency, JobResponse, JobVariable};
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::info;

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

impl<'a> Dependency<'a> {
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

    async fn download_to_file(&self, _file: &JobArtifactFile) -> Result<(), ClientError> {
        let mut path = self.job.build_dir.join("artifacts");
        if let Err(e) = tokio::fs::create_dir(&path).await {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(ClientError::WriteFailure(e));
            }
        }

        // TODO this assumes it's all zip artifacts
        path.push(format!("{}.zip", self.id()));
        let mut f = tokio::fs::File::create(&path)
            .await
            .map_err(ClientError::WriteFailure)?;
        self.job
            .client
            .download_artifact(self.dependency.id, &self.dependency.token, &mut f)
            .await?;
        self.job.artifacts.insert_file(self.dependency.id, path);
        Ok(())
    }

    async fn download_to_mem(&self, file: &JobArtifactFile) -> Result<(), ClientError> {
        let mut bytes = Vec::with_capacity(file.size);
        self.job
            .client
            .download_artifact(self.dependency.id, &self.dependency.token, &mut bytes)
            .await?;
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
    pub fn variable(&self, key: &str) -> Option<Variable> {
        self.response.variables.get(key).map(|v| Variable { v })
    }

    /// Get an iterator over all the variables associated with this job.
    pub fn variables(&self) -> impl Iterator<Item = Variable> {
        self.response.variables.values().map(|v| Variable { v })
    }

    /// Get an iterator over the job dependencies
    pub fn dependencies(&self) -> impl Iterator<Item = Dependency> {
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
}
