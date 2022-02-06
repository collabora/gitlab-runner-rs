use reqwest::multipart::{Form, Part};
use reqwest::{Body, StatusCode};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use url::Url;

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

const GITLAB_TRACE_UPDATE_INTERVAL: &str = "X-GitLab-Trace-Update-Interval";

#[derive(Debug, Clone, Serialize)]
struct JobRequest<'a> {
    token: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum JobState {
    Pending,
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename = "lower_case")]
struct JobUpdate<'a> {
    token: &'a str,
    state: JobState,
}

#[derive(Debug, Clone)]
pub struct JobUpdateReply {
    trace_update_interval: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct TraceReply {
    pub trace_update_interval: Option<Duration>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobVariable {
    pub key: String,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub value: String,
    pub public: bool,
    pub masked: bool,
}

fn variable_hash<'de, D>(deserializer: D) -> Result<HashMap<String, JobVariable>, D::Error>
where
    D: Deserializer<'de>,
{
    let hash = Vec::<JobVariable>::deserialize(deserializer)?
        .drain(..)
        .map(|v| (v.key.clone(), v))
        .collect();
    Ok(hash)
}

#[derive(Copy, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum JobStepWhen {
    Always,
    OnFailure,
    OnSuccess,
}

#[derive(Copy, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Script,
    AfterScript,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobStep {
    pub name: Phase,
    pub script: Vec<String>,
    pub timeout: u32,
    pub when: JobStepWhen,
    pub allow_failure: bool,
}

#[derive(Copy, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactWhen {
    Always,
    OnFailure,
    OnSuccess,
}

impl Default for ArtifactWhen {
    fn default() -> Self {
        Self::OnSuccess
    }
}

#[derive(Copy, Clone, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Zip,
    Gzip,
    Raw,
}

impl Default for ArtifactFormat {
    fn default() -> Self {
        Self::Zip
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobArtifact {
    pub name: Option<String>,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub untracked: bool,
    pub paths: Vec<String>,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub when: ArtifactWhen,
    pub artifact_type: String,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub artifact_format: ArtifactFormat,
    pub expire_in: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobArtifactFile {
    pub filename: String,
    pub size: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobDependency {
    pub id: u64,
    pub name: String,
    pub token: String,
    pub artifacts_file: Option<JobArtifactFile>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct JobResponse {
    pub id: u64,
    pub token: String,
    pub allow_git_fetch: bool,
    #[serde(deserialize_with = "variable_hash")]
    pub variables: HashMap<String, JobVariable>,
    pub steps: Vec<JobStep>,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub dependencies: Vec<JobDependency>,
    #[serde(deserialize_with = "deserialize_null_default")]
    pub artifacts: Vec<JobArtifact>,
    #[serde(flatten)]
    unparsed: JsonValue,
}

impl JobResponse {
    pub fn step(&self, name: Phase) -> Option<&JobStep> {
        self.steps.iter().find(|s| s.name == name)
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Unexpected reply code {0}")]
    UnexpectedStatus(StatusCode),
    #[error("Request failure {0}")]
    Request(#[from] reqwest::Error),
    #[error("Failed to write to destination {0}")]
    WriteFailure(#[source] futures::io::Error),
    #[error("Empty trace")]
    EmptyTrace,
}

#[derive(Clone, Debug)]
pub(crate) struct Client {
    client: reqwest::Client,
    url: Url,
    token: String,
}

impl Client {
    pub fn new(url: Url, token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            url,
            token,
        }
    }

    pub async fn request_job(&self) -> Result<Option<JobResponse>, Error> {
        let request = JobRequest { token: &self.token };

        let mut url = self.url.clone();
        url.path_segments_mut()
            .unwrap()
            .extend(&["api", "v4", "jobs", "request"]);

        let r = self
            .client
            .post(url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;

        match r.status() {
            StatusCode::CREATED => Ok(Some(r.json().await?)),
            StatusCode::NO_CONTENT => Ok(None),
            _ => Err(Error::UnexpectedStatus(r.status())),
        }
    }

    pub async fn update_job(
        &self,
        id: u64,
        token: &str,
        state: JobState,
    ) -> Result<JobUpdateReply, Error> {
        let mut url = self.url.clone();
        let id_s = format!("{}", id);
        url.path_segments_mut()
            .unwrap()
            .extend(&["api", "v4", "jobs", &id_s]);

        let update = JobUpdate { token, state };

        let r = self.client.put(url).json(&update).send().await?;
        let trace_update_interval = r
            .headers()
            .get(GITLAB_TRACE_UPDATE_INTERVAL)
            .and_then(|v| Some(Duration::from_secs(v.to_str().ok()?.parse().ok()?)));
        match r.status() {
            StatusCode::OK => Ok(JobUpdateReply {
                trace_update_interval,
            }),
            _ => Err(Error::UnexpectedStatus(r.status())),
        }
    }

    pub async fn trace<B>(
        &self,
        id: u64,
        token: &str,
        body: B,
        start: usize,
        length: usize,
    ) -> Result<TraceReply, Error>
    where
        B: Into<Body>,
    {
        if length == 0 {
            return Err(Error::EmptyTrace);
        }

        let mut url = self.url.clone();
        let id_s = format!("{}", id);
        url.path_segments_mut()
            .unwrap()
            .extend(&["api", "v4", "jobs", &id_s, "trace"]);

        let range = format!("{}-{}", start, start + length - 1);

        let r = self
            .client
            .patch(url)
            .header("JOB-TOKEN", token)
            .header(reqwest::header::CONTENT_RANGE, range)
            .body(body)
            .send()
            .await?;

        let trace_update_interval = r
            .headers()
            .get(GITLAB_TRACE_UPDATE_INTERVAL)
            .and_then(|v| Some(Duration::from_secs(v.to_str().ok()?.parse().ok()?)));

        match r.status() {
            StatusCode::ACCEPTED => Ok(TraceReply {
                trace_update_interval,
            }),
            _ => Err(Error::UnexpectedStatus(r.status())),
        }
    }

    pub async fn download_artifact<D: AsyncWrite + Unpin>(
        &self,
        id: u64,
        token: &str,
        mut dest: D,
    ) -> Result<(), Error> {
        let mut url = self.url.clone();
        let id_s = format!("{}", id);
        url.path_segments_mut()
            .unwrap()
            .extend(&["api", "v4", "jobs", &id_s, "artifacts"]);

        let mut r = self
            .client
            .get(url)
            .header("JOB-TOKEN", token)
            .send()
            .await?;

        match r.status() {
            StatusCode::OK => {
                while let Some(ref chunk) = r.chunk().await? {
                    dest.write_all(chunk).await.map_err(Error::WriteFailure)?
                }
                Ok(())
            }
            _ => Err(Error::UnexpectedStatus(r.status())),
        }
    }

    pub async fn upload_artifact<D>(
        &self,
        id: u64,
        token: &str,
        name: &str,
        data: D,
    ) -> Result<(), Error>
    where
        D: Into<Body>,
    {
        let part = Part::stream(data).file_name(name.to_string());
        let form = Form::new().part("file", part);

        let mut url = self.url.clone();
        let id_s = format!("{}", id);
        url.path_segments_mut()
            .unwrap()
            .extend(&["api", "v4", "jobs", &id_s, "artifacts"]);

        let r = self
            .client
            .post(url)
            .header("JOB-TOKEN", token)
            .multipart(form)
            .send()
            .await?;

        match r.status() {
            StatusCode::CREATED => Ok(()),
            _ => Err(Error::UnexpectedStatus(r.status())),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use gitlab_runner_mock::GitlabRunnerMock;
    use serde_json::json;

    #[test]
    fn deserialize_variables() {
        #[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
        struct Test {
            #[serde(deserialize_with = "variable_hash")]
            variables: HashMap<String, JobVariable>,
        }

        let json = json!({
            "variables": [
                { "key": "VAR1", "value": "1", "public": true, "masked": false },
                { "key": "VAR2", "value": "2", "public": false, "masked": true }
            ]
        });

        let t: Test = serde_json::from_str(&json.to_string()).expect("Failed to deserialize json");
        assert_eq!(2, t.variables.len());
        let v = t.variables.get("VAR1").unwrap();
        assert_eq!(
            &JobVariable {
                key: "VAR1".to_string(),
                value: "1".to_string(),
                public: true,
                masked: false
            },
            v
        );
        let v = t.variables.get("VAR2").unwrap();
        assert_eq!(
            &JobVariable {
                key: "VAR2".to_string(),
                value: "2".to_string(),
                public: false,
                masked: true
            },
            v
        );
    }

    #[tokio::test]
    async fn no_job() {
        let mock = GitlabRunnerMock::start().await;

        let client = Client::new(mock.uri(), mock.runner_token().to_string());

        let job = client.request_job().await.unwrap();

        assert_eq!(None, job);
    }

    #[tokio::test]
    async fn process_job() {
        let mock = GitlabRunnerMock::start().await;
        mock.add_dummy_job("process job".to_string());

        let client = Client::new(mock.uri(), mock.runner_token().to_string());

        if let Some(job) = client.request_job().await.unwrap() {
            client
                .update_job(job.id, &job.token, JobState::Success)
                .await
                .unwrap();
        } else {
            panic!("No job!")
        }

        let job = client.request_job().await.unwrap();
        assert_eq!(None, job);
    }
}
