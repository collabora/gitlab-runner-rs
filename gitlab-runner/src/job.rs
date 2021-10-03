use crate::artifact::Artifact;
use crate::client::{Client, JobDependency, JobResponse, JobVariable};
use bytes::{Bytes, BytesMut};
use std::sync::{Arc, Mutex};

pub struct Variable<'a> {
    v: &'a JobVariable,
}

impl<'a> Variable<'a> {
    pub fn key(&self) -> &'a str {
        &self.v.key
    }

    pub fn value(&self) -> &'a str {
        &self.v.value
    }

    pub fn masked(&self) -> bool {
        self.v.masked
    }
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

#[derive(Debug)]
pub struct Dependency<'a> {
    job: &'a Job,
    dependency: &'a JobDependency,
}

impl<'a> Dependency<'a> {
    pub fn id(&self) -> u64 {
        self.dependency.id
    }

    pub fn name(&self) -> &str {
        &self.dependency.name
    }

    pub fn artifact_filename(&self) -> Option<&str> {
        self.dependency
            .artifacts_file
            .as_ref()
            .map(|a| a.filename.as_str())
    }

    pub fn artifact_size(&self) -> Option<usize> {
        self.dependency.artifacts_file.as_ref().map(|a| a.size)
    }

    pub async fn download(&self) -> Result<Option<Artifact>, crate::client::Error> {
        if self.dependency.artifacts_file.is_some() {
            let bytes = self
                .job
                .client
                .download_artifact(self.dependency.id, &self.dependency.token)
                .await?;
            Ok(Some(Artifact::new(bytes)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JobData {
    trace: Arc<Mutex<BytesMut>>,
}

impl JobData {
    fn new() -> Self {
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

#[derive(Debug)]
pub struct Job {
    response: Arc<JobResponse>,
    client: Client,
    data: JobData,
}

impl Job {
    pub(crate) fn new(client: Client, response: Arc<JobResponse>) -> (Self, JobData) {
        let data = JobData::new();
        (
            Self {
                client,
                response,
                data: data.clone(),
            },
            data,
        )
    }

    pub fn id(&self) -> u64 {
        self.response.id
    }

    pub fn trace<D: AsRef<[u8]>>(&self, data: D) {
        self.data.trace(data.as_ref());
    }

    pub fn variable(&self, key: &str) -> Option<Variable> {
        self.response.variables.get(key).map(|v| Variable { v })
    }

    pub fn dependencies(&self) -> impl Iterator<Item = Dependency> {
        self.response
            .dependencies
            .iter()
            .map(move |dependency| Dependency {
                job: self,
                dependency,
            })
    }
}
