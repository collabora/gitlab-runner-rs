use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;

use crate::variables::default_job_variables;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MockJobState {
    Pending,
    Running,
    Success,
    Failed,
    Cancelled,
}

impl MockJobState {
    pub fn finished(self) -> bool {
        self == Self::Success || self == Self::Failed
    }
}

impl ToString for MockJobState {
    fn to_string(&self) -> String {
        match *self {
            MockJobState::Pending => "pending",
            MockJobState::Running => "running",
            MockJobState::Success => "success",
            MockJobState::Failed => "failed",
            // The spelling mismatch of "cancelled" vs "canceled" is
            // intentional: this crate, as well as tokio_util, already use
            // "cancelled", so using it here keeps the spelling consistent, even
            // if it's not *identical* to the exact GitLab job status.
            MockJobState::Cancelled => "canceled",
        }
        .to_owned()
    }
}

#[derive(Debug, Error)]
pub enum LogError {
    #[error("Incorrect range start")]
    IncorrectStart,
    #[error("Incorrect range end")]
    IncorrectEnd,
}

#[derive(Debug)]
pub(crate) struct MockJobInner {
    state: MockJobState,
    state_updates: u32,
    artifact: Arc<Vec<u8>>,
    log: Vec<Vec<u8>>,
    log_patches: u32,
}

#[derive(Clone, Serialize, Debug)]
pub struct MockJobVariable {
    pub key: String,
    pub value: String,
    pub public: bool,
    pub masked: bool,
}

#[derive(Copy, Clone, Serialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MockJobStepWhen {
    Always,
    OnFailure,
    OnSuccess,
}

#[derive(Copy, Clone, Serialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MockJobStepName {
    Script,
    AfterScript,
}

#[derive(Clone, Serialize, Debug)]
pub struct MockJobStep {
    name: MockJobStepName,
    script: Vec<String>,
    timeout: u64,
    when: MockJobStepWhen,
    allow_failure: bool,
}

#[derive(Copy, Clone, Serialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MockJobArtifactWhen {
    Always,
    OnFailure,
    OnSuccess,
}

#[derive(Clone, Serialize, Debug)]
pub struct MockJobArtifact {
    pub name: Option<String>,
    pub untracked: bool,
    pub paths: Vec<String>,
    pub when: Option<MockJobArtifactWhen>,
    pub artifact_type: String,
    pub artifact_format: Option<String>,
    pub expire_in: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MockJob {
    name: String,
    id: u64,
    token: String,
    variables: Vec<MockJobVariable>,
    steps: Vec<MockJobStep>,
    dependencies: Vec<MockJob>,
    artifacts: Vec<MockJobArtifact>,
    token_prefixes: Vec<String>,
    inner: Arc<Mutex<MockJobInner>>,
}

impl MockJob {
    pub(crate) fn new(name: String, id: u64) -> Self {
        let mut builder = MockJobBuilder::new(name, id);
        builder.add_step(
            MockJobStepName::Script,
            vec!["dummy".to_string()],
            3600,
            MockJobStepWhen::OnSuccess,
            false,
        );

        builder.build()
    }

    pub(crate) fn new_completed(name: String, id: u64, artifact: Vec<u8>) -> Self {
        Self {
            name,
            id,
            token: format!("job-token-{}", id),
            variables: Vec::new(),
            steps: Vec::new(),
            dependencies: Vec::new(),
            artifacts: Vec::new(),
            token_prefixes: Vec::new(),
            inner: Arc::new(Mutex::new(MockJobInner {
                state: MockJobState::Success,
                state_updates: 2,
                artifact: Arc::new(artifact),
                log: Vec::new(),
                log_patches: 0,
            })),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn dependencies(&self) -> &[MockJob] {
        &self.dependencies
    }

    pub fn artifacts(&self) -> &[MockJobArtifact] {
        &self.artifacts
    }

    pub fn variables(&self) -> &[MockJobVariable] {
        &self.variables
    }

    pub fn token_prefixes(&self) -> &[String] {
        &self.token_prefixes
    }

    pub fn steps(&self) -> &[MockJobStep] {
        &self.steps
    }

    pub fn state(&self) -> MockJobState {
        let inner = self.inner.lock().unwrap();
        inner.state
    }

    pub fn state_updates(&self) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner.state_updates
    }

    pub fn finished(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.state.finished()
    }

    pub fn log_last(&self) -> Option<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        inner.log.last().cloned()
    }

    pub fn log(&self) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        inner.log.concat()
    }

    pub fn log_patches(&self) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner.log_patches
    }

    pub fn artifact(&self) -> Arc<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        inner.artifact.clone()
    }

    pub fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        assert!(!inner.state.finished(), "Job is already finished");
        inner.state_updates += 1;
        inner.state = MockJobState::Cancelled;
    }

    pub(crate) fn update_state(&self, state: MockJobState) {
        let mut inner = self.inner.lock().unwrap();
        inner.state_updates += 1;
        inner.state = state;
    }

    pub(crate) fn append_log(
        &self,
        data: Vec<u8>,
        start: usize,
        end: usize,
    ) -> Result<(), LogError> {
        let mut inner = self.inner.lock().unwrap();

        let log_len = inner.log.iter().fold(0, |acc, l| acc + l.len());
        if log_len != start {
            return Err(LogError::IncorrectStart);
        }

        if start + data.len() - 1 != end {
            return Err(LogError::IncorrectEnd);
        }

        inner.log.push(data);
        inner.log_patches += 1;
        Ok(())
    }

    pub(crate) fn set_artifact(&self, data: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        assert!(inner.artifact.is_empty());
        inner.artifact = Arc::new(data);
    }
}

#[derive(Debug, Default)]
pub struct MockJobBuilder {
    name: String,
    id: u64,
    variables: HashMap<String, MockJobVariable>,
    steps: Vec<MockJobStep>,
    dependencies: Vec<MockJob>,
    token_prefixes: Vec<String>,
    artifacts: Vec<MockJobArtifact>,
}

impl MockJobBuilder {
    pub(crate) fn new(name: String, id: u64) -> Self {
        Self {
            name,
            id,
            variables: default_job_variables(id)
                .into_iter()
                .map(|v| (v.key.clone(), v))
                .collect(),
            ..Default::default()
        }
    }

    pub fn add_variable(&mut self, key: String, value: String, public: bool, masked: bool) {
        self.variables.insert(
            key.clone(),
            MockJobVariable {
                key,
                value,
                public,
                masked,
            },
        );
    }

    pub fn add_token_prefix(&mut self, prefix: String) {
        self.token_prefixes.push(prefix);
    }

    pub fn add_step(
        &mut self,
        name: MockJobStepName,
        script: Vec<String>,
        timeout: u64,
        when: MockJobStepWhen,
        allow_failure: bool,
    ) {
        if self.steps.iter().any(|s| s.name == name) {
            panic!("Step already exists!");
        }

        let step = MockJobStep {
            name,
            script,
            timeout,
            when,
            allow_failure,
        };

        self.steps.push(step);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_artifact(
        &mut self,
        name: Option<String>,
        untracked: bool,
        paths: Vec<String>,
        when: Option<MockJobArtifactWhen>,
        artifact_type: String,
        artifact_format: Option<String>,
        expire_in: Option<String>,
    ) {
        self.artifacts.push(MockJobArtifact {
            name,
            untracked,
            paths,
            when,
            artifact_type,
            artifact_format,
            expire_in,
        });
    }

    pub fn add_artifact_paths(&mut self, paths: Vec<String>) {
        self.add_artifact(
            None,
            false,
            paths,
            None,
            "archive".to_string(),
            Some("zip".to_string()),
            None,
        );
    }

    pub fn dependency(&mut self, dependency: MockJob) {
        self.dependencies.push(dependency);
    }

    pub fn build(self) -> MockJob {
        assert!(!self.steps.is_empty(), "Should have at least one step");
        let inner = MockJobInner {
            state: MockJobState::Pending,
            state_updates: 0,
            log: Vec::new(),
            log_patches: 0,
            artifact: Arc::default(),
        };

        let inner = Arc::new(Mutex::new(inner));
        MockJob {
            name: self.name,
            id: self.id,
            token: format!("job-token-{}", self.id),
            steps: self.steps,
            variables: self.variables.into_values().collect(),
            dependencies: self.dependencies,
            artifacts: self.artifacts,
            token_prefixes: self.token_prefixes,
            inner,
        }
    }
}
