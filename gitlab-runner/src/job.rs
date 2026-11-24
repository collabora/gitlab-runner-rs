//! This module describes a single gitlab job
use crate::artifact::Artifact;
use crate::client::{Client, JobArtifactFile, JobDependency, JobResponse, JobVariable};
use crate::outputln;
use bytes::{Bytes, BytesMut};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio_retry2::strategy::{FibonacciBackoff, jitter};
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

/// A download destination that can be emptied again before a retry
trait ArtifactWriter: AsyncWrite + Unpin {
    async fn reset(&mut self) -> std::io::Result<()>;
}

impl ArtifactWriter for Vec<u8> {
    async fn reset(&mut self) -> std::io::Result<()> {
        self.clear();
        Ok(())
    }
}

impl ArtifactWriter for tokio::fs::File {
    async fn reset(&mut self) -> std::io::Result<()> {
        self.set_len(0).await?;
        self.rewind().await?;
        Ok(())
    }
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

    async fn download_impl<A>(&self, writer: &mut A) -> Result<(), ClientError>
    where
        A: ArtifactWriter,
    {
        let mut strategy = FibonacciBackoff::from_millis(900).map(jitter).take(4);
        loop {
            let r = self
                .job
                .client
                .download_artifact(self.dependency.id, &self.dependency.token, &mut *writer)
                .await;

            match (r, strategy.next()) {
                (Err(err), Some(d)) if is_retriable_error(&err) => {
                    outputln!(
                        "Error getting artifacts from {}: {err}. Retrying",
                        self.dependency.name
                    );
                    tokio::time::sleep(d).await;
                    // A failed attempt may have written part of the body already.
                    writer.reset().await.map_err(ClientError::WriteFailure)?;
                }
                (Ok(()), _) => {
                    // Wait for pending writes before the caller caches the artifact.
                    return writer.flush().await.map_err(ClientError::WriteFailure);
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
            // Concurrent downloads of one artifact would write to the same destination.
            let _guard = self.job.artifacts.lock(self.dependency.id).await;
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
    downloads: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
}

impl ArtifactCache {
    fn new() -> Self {
        ArtifactCache {
            data: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
        }
    }

    /// Wait until no other task is downloading the artifact with the given id
    async fn lock(&self, id: u64) -> tokio::sync::OwnedMutexGuard<()> {
        let download = self
            .downloads
            .lock()
            .unwrap()
            .entry(id)
            .or_default()
            .clone();
        download.lock_owned().await
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
}

#[cfg(test)]
mod tests {
    use super::{ArtifactWriter, Job, JobLog};
    use crate::client::{Client, ClientMetadata};
    use gitlab_runner_mock::{GitlabRunnerMock, MockJobStepName, MockJobStepWhen};
    use std::io::{Cursor, Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::fs::File;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
    use tokio::net::TcpListener;
    use zip::{ZipWriter, write::SimpleFileOptions};

    impl ArtifactWriter for BufWriter<File> {
        async fn reset(&mut self) -> std::io::Result<()> {
            self.flush().await?;
            self.get_mut().reset().await
        }
    }

    /// Answer one request per entry with the given raw bytes, then close the connection.
    async fn serve_replies(listener: TcpListener, responses: Vec<Vec<u8>>) {
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    }

    /// Answer every request with the given raw bytes and count the requests.
    async fn serve_counting(listener: TcpListener, response: Vec<u8>, requests: Arc<AtomicUsize>) {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let response = response.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                requests.fetch_add(1, Ordering::SeqCst);
                stream.write_all(&response).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    }

    #[tokio::test]
    async fn download_flushes_zip_before_reopening() {
        let mock = GitlabRunnerMock::start().await;
        let definition = b"job_name: probe\nactions: []\n";
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("lava/testjob.yaml", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(definition).unwrap();
        let archive = archive.finish().unwrap().into_inner();
        let archive_len = archive.len();
        let producer = mock.add_completed_job("producer".to_owned(), archive);
        let mut consumer = mock.job_builder("consumer".to_owned());
        consumer.dependency(producer);
        consumer.add_step(
            MockJobStepName::Script,
            vec!["dummy".to_owned()],
            3600,
            MockJobStepWhen::OnSuccess,
            false,
        );
        mock.enqueue_job(consumer.build());

        let client = Client::new(
            mock.uri(),
            mock.runner_token().to_owned(),
            "s_ystem_id1234".to_owned(),
            ClientMetadata::default(),
        );
        let response = client.request_job().await.unwrap().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let job = Job::new(client, Arc::new(response), dir.path().into(), JobLog::new());
        let dependency = job.dependencies().next().unwrap();

        let path = dir.path().join("artifact.zip");
        let file = File::create(&path).await.unwrap();
        // Hold the entire ZIP, including its end record, until the download flushes.
        let mut writer = BufWriter::with_capacity(archive_len + 1, file);
        dependency.download_impl(&mut writer).await.unwrap();

        job.artifacts.insert_file(dependency.id(), path);
        let mut artifact = job
            .artifacts
            .get(dependency.id())
            .await
            .expect("The downloaded ZIP must be readable after reopening")
            .unwrap();
        let mut contents = Vec::new();
        artifact
            .file("lava/testjob.yaml")
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, definition);
    }

    #[tokio::test]
    async fn retry_discards_partial_download() {
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let header = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
        // The first reply ends halfway through the body, the second one is complete.
        let truncated = [header.as_bytes(), &body[..body.len() / 2]].concat();
        let complete = [header.as_bytes(), &body[..]].concat();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(serve_replies(listener, vec![truncated, complete]));

        let response = serde_json::from_value(serde_json::json!({
            "id": 2,
            "token": "job-token",
            "allow_git_fetch": false,
            "variables": [],
            "steps": [],
            "dependencies": [{
                "id": 1,
                "name": "producer",
                "token": "dependency-token",
                "artifacts_file": { "filename": "artifacts.zip", "size": body.len() },
            }],
            "artifacts": [],
        }))
        .unwrap();
        let client = Client::new(
            url.parse().unwrap(),
            "runner-token".to_owned(),
            "s_ystem_id1234".to_owned(),
            ClientMetadata::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let job = Job::new(client, Arc::new(response), dir.path().into(), JobLog::new());
        let dependency = job.dependencies().next().unwrap();

        let mut downloaded = Vec::new();
        dependency.download_impl(&mut downloaded).await.unwrap();

        server.await.unwrap();
        assert_eq!(downloaded, body);
    }

    #[tokio::test]
    async fn file_reset_truncates_and_rewinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.zip");
        let mut file = tokio::fs::File::create(&path).await.unwrap();

        file.write_all(b"partial download").await.unwrap();
        file.reset().await.unwrap();
        assert_eq!(file.stream_position().await.unwrap(), 0);
        file.write_all(b"complete").await.unwrap();
        file.flush().await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"complete");
    }

    #[tokio::test]
    async fn concurrent_downloads_share_one_request() {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("test", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"testdata").unwrap();
        let archive = archive.finish().unwrap().into_inner();
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            archive.len()
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(serve_counting(
            listener,
            [header.as_bytes(), &archive[..]].concat(),
            requests.clone(),
        ));

        let response = serde_json::from_value(serde_json::json!({
            "id": 2,
            "token": "job-token",
            "allow_git_fetch": false,
            "variables": [],
            "steps": [],
            "dependencies": [{
                "id": 1,
                "name": "producer",
                "token": "dependency-token",
                "artifacts_file": { "filename": "artifacts.zip", "size": archive.len() },
            }],
            "artifacts": [],
        }))
        .unwrap();
        let client = Client::new(
            url.parse().unwrap(),
            "runner-token".to_owned(),
            "s_ystem_id1234".to_owned(),
            ClientMetadata::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let job = Job::new(client, Arc::new(response), dir.path().into(), JobLog::new());
        let first = job.dependencies().next().unwrap();
        let second = job.dependencies().next().unwrap();

        let (first, second) = tokio::join!(first.download(), second.download());

        server.abort();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        for artifact in [first, second] {
            let mut artifact = artifact.unwrap().unwrap();
            let mut contents = Vec::new();
            artifact
                .file("test")
                .unwrap()
                .read_to_end(&mut contents)
                .unwrap();
            assert_eq!(contents, b"testdata");
        }
    }
}
