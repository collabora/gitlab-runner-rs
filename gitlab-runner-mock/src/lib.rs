// Required for serde_json::json macro on the massive job json
#![recursion_limit = "256"]

use std::sync::Arc;
use std::sync::Mutex;
use url::Url;
use wiremock::matchers::{body_json_schema, method, path, path_regex};
use wiremock::{Mock, MockServer};

mod job;
mod variables;
pub use job::{
    MockJob, MockJobArtifactWhen, MockJobBuilder, MockJobState, MockJobStepName, MockJobStepWhen,
};

mod api;
use api::JobArtifactsDownloader;
use api::JobArtifactsUploader;
use api::JobTraceResponder;
use api::JobUpdateResponder;
use api::{JobRequest, JobRequestResponder};

#[derive(Default)]
struct JobData {
    last_id: u64,
    jobs: Vec<MockJob>,
}

struct Inner {
    server: MockServer,
    runner_token: String,
    jobs: Mutex<JobData>,
    update_interval: Mutex<u32>,
}

#[derive(Clone)]
pub struct GitlabRunnerMock {
    inner: Arc<Inner>,
}

impl GitlabRunnerMock {
    pub async fn start() -> Self {
        let m = MockServer::start().await;
        let jobs = JobData {
            last_id: 265,
            ..Default::default()
        };
        let inner = Inner {
            server: m,
            runner_token: "fakerunnertoken".to_string(),
            jobs: Mutex::new(jobs),
            update_interval: Mutex::new(3),
        };
        let server = Self {
            inner: Arc::new(inner),
        };

        Mock::given(method("POST"))
            .and(path("api/v4/jobs/request"))
            .and(body_json_schema::<JobRequest>)
            .respond_with(JobRequestResponder::new(server.clone()))
            .mount(&server.inner.server)
            .await;

        Mock::given(method("PUT"))
            .and(path_regex(r"^/api/v4/jobs/\d+$"))
            //.and(body_json_schema::<JobUpdate>)
            .respond_with(JobUpdateResponder::new(server.clone()))
            .mount(&server.inner.server)
            .await;

        Mock::given(method("PATCH"))
            .and(path_regex(r"^/api/v4/jobs/\d+/trace"))
            .respond_with(JobTraceResponder::new(server.clone()))
            .mount(&server.inner.server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/api/v4/jobs/\d+/artifacts"))
            .respond_with(JobArtifactsUploader::new(server.clone()))
            .mount(&server.inner.server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(r"^/api/v4/jobs/\d+/artifacts"))
            .respond_with(JobArtifactsDownloader::new(server.clone()))
            .mount(&server.inner.server)
            .await;

        server
    }

    pub fn uri(&self) -> Url {
        self.inner.server.uri().parse().expect("uri is not a url")
    }

    pub fn runner_token(&self) -> &str {
        &self.inner.runner_token
    }

    pub fn add_dummy_job(&self, name: String) -> MockJob {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.last_id += 1;

        let job = MockJob::new(name, jobs.last_id);
        jobs.jobs.push(job.clone());
        job
    }

    pub fn add_completed_job(&self, name: String, artifact: Vec<u8>) -> MockJob {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.last_id += 1;

        let job = MockJob::new_completed(name, jobs.last_id, artifact);

        jobs.jobs.push(job.clone());

        job
    }

    pub fn job_builder(&self, name: String) -> MockJobBuilder {
        let mut jobs = self.inner.jobs.lock().unwrap();
        jobs.last_id += 1;

        MockJobBuilder::new(name, jobs.last_id)
    }

    pub fn enqueue_job(&self, job: MockJob) {
        let mut jobs = self.inner.jobs.lock().unwrap();
        if jobs.jobs.iter().any(|j| j.id() == job.id()) {
            panic!("Trying to requeue already queued job");
        }

        jobs.jobs.push(job);
    }

    pub fn get_job_artifact(&self, id: u64) -> Option<Vec<u8>> {
        let jobs = self.inner.jobs.lock().unwrap();

        jobs.jobs
            .iter()
            .find(|j| j.id() == id)
            .map(|j| j.artifact().as_slice().to_vec())
    }

    fn grab_pending_job(&self) -> Option<MockJob> {
        let jobs = self.inner.jobs.lock().unwrap();
        for job in jobs.jobs.iter() {
            if job.state() == MockJobState::Pending
                && job.dependencies().iter().all(|d| d.state().finished())
            {
                return Some(job.clone());
            }
        }
        None
    }

    pub fn get_job(&self, id: u64) -> Option<MockJob> {
        let jobs = self.inner.jobs.lock().unwrap();
        for job in jobs.jobs.iter() {
            if job.id() == id {
                return Some(job.clone());
            }
        }
        None
    }

    pub async fn n_requests(&self) -> usize {
        let requests = self.inner.server.received_requests().await.unwrap();
        requests
            .iter()
            .filter(|r| r.url.path() == "/api/v4/jobs/request")
            .count()
    }

    pub fn update_interval(&self) -> u32 {
        *self.inner.update_interval.lock().unwrap()
    }

    pub fn set_update_interval(&self, interval: u32) {
        *self.inner.update_interval.lock().unwrap() = interval;
    }
}
