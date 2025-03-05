use std::borrow::Cow;
use std::io::Read;

use flate2::read::GzDecoder;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::Registry;
use zip::ZipArchive;

use gitlab_runner::job::Job;
use gitlab_runner::{GitlabLayer, JobHandler, JobResult, Phase, RunnerBuilder, UploadableFile};
use gitlab_runner_mock::{
    GitlabRunnerMock, MockJobArtifactWhen, MockJobState, MockJobStepName, MockJobStepWhen,
};

#[derive(PartialEq, Eq)]
enum TestFile {
    Test,
    Test2,
}

#[async_trait::async_trait]
impl UploadableFile for TestFile {
    type Data<'a> = &'a [u8];

    fn get_path(&self) -> Cow<'_, str> {
        match self {
            TestFile::Test => "test".into(),
            TestFile::Test2 => "test2".into(),
        }
    }
    async fn get_data(&self) -> Result<&'_ [u8], ()> {
        Ok(match self {
            TestFile::Test => b"testdata".as_slice(),
            TestFile::Test2 => b"testdata2".as_slice(),
        })
    }
}

struct Upload();

#[async_trait::async_trait]
impl JobHandler<TestFile> for Upload {
    async fn step(&mut self, _script: &[String], _phase: Phase) -> JobResult {
        Ok(())
    }

    async fn get_uploadable_files(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = TestFile> + Send>, ()> {
        Ok(Box::new([TestFile::Test, TestFile::Test2].into_iter()))
    }
}

struct Download();

impl Download {
    async fn new(job: Job) -> Self {
        assert_eq!(job.dependencies().count(), 1);
        for d in job.dependencies() {
            let mut artifact = d
                .download()
                .await
                .unwrap()
                .expect("No artifacts to download");
            let mut names: Vec<_> = artifact.file_names().collect();
            names.sort_unstable();
            assert_eq!(2, names.len());
            assert_eq!(&["test", "test2"], names.as_slice());

            let mut f = artifact.file("test").expect("Testfile not available");
            assert_eq!(f.name(), "test");
            let mut data = Vec::new();
            f.read_to_end(&mut data).expect("Failed to read content");
            assert_eq!("testdata".as_bytes(), &*data);
            drop(f);

            let mut f = artifact.file("test2").expect("Testfile not available");
            assert_eq!(f.name(), "test2");
            let mut data = Vec::new();
            f.read_to_end(&mut data).expect("Failed to read content");
            assert_eq!("testdata2".as_bytes(), &*data);
            drop(f);

            /* Try reading first data file a second time */
            let mut f = artifact.file("test").expect("Testfile not available");
            assert_eq!(f.name(), "test");
            let mut data = Vec::new();
            f.read_to_end(&mut data).expect("Failed to read content");
            assert_eq!("testdata".as_bytes(), &*data);
        }
        Download()
    }
}

#[async_trait::async_trait]
impl JobHandler for Download {
    async fn step(&mut self, _script: &[String], _phase: Phase) -> JobResult {
        Ok(())
    }
}

#[tokio::test]
async fn upload_download() {
    let mock = GitlabRunnerMock::start().await;
    let mut upload = mock.job_builder("upload artifact".to_string());
    upload.add_step(
        MockJobStepName::Script,
        vec!["dummy".to_string()],
        3600,
        MockJobStepWhen::OnSuccess,
        false,
    );
    upload.add_artifact_paths(vec!["*".to_string()]);
    let upload = upload.build();
    mock.enqueue_job(upload.clone());

    let mut download = mock.job_builder("download artifact".to_string());
    download.dependency(upload.clone());
    download.add_step(
        MockJobStepName::Script,
        vec!["dummy".to_string()],
        3600,
        MockJobStepWhen::OnSuccess,
        false,
    );

    let download = download.build();

    mock.enqueue_job(download.clone());

    let dir = tempfile::tempdir().unwrap();
    let (layer, jobs) = GitlabLayer::new();
    let subscriber = Registry::default().with(layer);
    let mut runner = RunnerBuilder::new(mock.uri(), mock.runner_token(), dir.path(), jobs)
        .build()
        .await;

    async {
        // Upload job comes first
        let got_job = runner
            .request_job(|_job| async move { Ok(Upload()) })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, upload.state());

        let got_job = runner
            .request_job(|job| async move { Ok(Download::new(job).await) })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, download.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn multiple_upload() {
    let mock = GitlabRunnerMock::start().await;
    let mut upload = mock.job_builder("multiple uploads".to_string());

    upload.add_step(
        MockJobStepName::Script,
        vec!["dummy".to_string()],
        3600,
        MockJobStepWhen::OnSuccess,
        false,
    );

    upload.add_artifact(
        None,
        false,
        vec!["*".to_string()],
        Some(MockJobArtifactWhen::OnSuccess),
        "archive".to_string(),
        Some("zip".to_string()),
        None,
    );

    upload.add_artifact(
        Some("junit".to_string()),
        false,
        vec!["test2".to_string()],
        Some(MockJobArtifactWhen::Always),
        "junit".to_string(),
        Some("gzip".to_string()),
        None,
    );

    let upload = upload.build();
    mock.enqueue_job(upload.clone());

    let mut download = mock.job_builder("download artifact".to_string());
    download.dependency(upload.clone());
    download.add_step(
        MockJobStepName::Script,
        vec!["dummy".to_string()],
        3600,
        MockJobStepWhen::OnSuccess,
        false,
    );

    let download = download.build();

    mock.enqueue_job(download.clone());

    let dir = tempfile::tempdir().unwrap();
    let (layer, jobs) = GitlabLayer::new();
    let subscriber = Registry::default().with(layer);
    let mut runner = RunnerBuilder::new(mock.uri(), mock.runner_token(), dir.path(), jobs)
        .build()
        .await;

    async {
        let got_job = runner
            .request_job(|_job| async move { Ok(Upload()) })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, upload.state());

        let mut saw_zip = false;
        let mut saw_gzip = false;
        for artifact in upload.uploaded_artifacts() {
            match artifact.filename.as_deref() {
                Some("default.zip") => {
                    assert_eq!(artifact.artifact_format.as_deref(), Some("zip"));
                    assert_eq!(artifact.artifact_type.as_deref(), Some("archive"));
                    let mut z = ZipArchive::new(std::io::Cursor::new(artifact.data.as_slice()))
                        .expect("failed to open zip archive");
                    saw_zip = true;
                    assert_eq!(z.len(), 2);
                    for ix in 0..z.len() {
                        let mut f = z.by_index(ix).expect("failed to get zip file");
                        match f.name() {
                            "test" => {
                                let mut data = Vec::new();
                                f.read_to_end(&mut data).expect("failed to read zip file");
                                assert_eq!(b"testdata", data.as_slice());
                            }
                            "test2" => {
                                let mut data = Vec::new();
                                f.read_to_end(&mut data).expect("failed to read zip file");
                                assert_eq!(b"testdata2", data.as_slice());
                            }
                            _ => {
                                unreachable!("unknown file in archive");
                            }
                        }
                    }
                }
                Some("junit.gz") => {
                    assert_eq!(artifact.artifact_format.as_deref(), Some("gzip"));
                    assert_eq!(artifact.artifact_type.as_deref(), Some("junit"));
                    saw_gzip = true;
                    let mut gz = GzDecoder::new(std::io::Cursor::new(artifact.data.as_slice()));
                    let mut data = Vec::new();
                    gz.read_to_end(&mut data).expect("failed to read gzip file");
                    assert_eq!(b"testdata2", data.as_slice());
                }
                _ => {
                    unreachable!("unknown artifact type in archive");
                }
            }
        }

        assert!(saw_zip, "No zip file was uploaded");
        assert!(saw_gzip, "No gzip file was uploaded");

        let got_job = runner
            .request_job(|job| async move { Ok(Download::new(job).await) })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, download.state());
    }
    .with_subscriber(subscriber)
    .await;
}
