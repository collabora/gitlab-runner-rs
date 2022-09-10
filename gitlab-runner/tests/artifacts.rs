use std::io::Read;

use futures::AsyncWriteExt;
use gitlab_runner::job::Job;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{CancelClient, JobHandler, JobResult, Phase, Runner};
use gitlab_runner_mock::{GitlabRunnerMock, MockJobState, MockJobStepName, MockJobStepWhen};

struct Upload();

#[async_trait::async_trait]
impl JobHandler for Upload {
    async fn step(
        &mut self,
        _script: &[String],
        _phase: Phase,
        _cancel: CancelClient,
    ) -> JobResult {
        Ok(())
    }

    async fn upload_artifacts(&mut self, uploader: &mut Uploader) -> JobResult {
        let mut f = uploader.file("test".to_string()).await;
        f.write_all(b"testdata")
            .await
            .expect("Couldn't write test data");
        drop(f);
        let mut f = uploader.file("test2".to_string()).await;
        f.write_all(b"testdata2")
            .await
            .expect("Couldn't write test data");
        Ok(())
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
    async fn step(
        &mut self,
        _script: &[String],
        _phase: Phase,
        _cancel: CancelClient,
    ) -> JobResult {
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

    let mut runner = Runner::new(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

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
