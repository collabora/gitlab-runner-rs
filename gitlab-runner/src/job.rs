use crate::artifact::Artifact;
use crate::client::{Client, JobDependency, JobResponse, JobVariable};
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc;

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

#[derive(Debug)]
pub(crate) enum JobRequest {
    Trace(Bytes),
}

#[derive(Debug)]
pub struct Job {
    data: Arc<JobResponse>,
    channel: mpsc::Sender<JobRequest>,
    client: Client,
}

impl Job {
    pub(crate) fn new(
        client: Client,
        data: Arc<JobResponse>,
        channel: mpsc::Sender<JobRequest>,
    ) -> Self {
        Self {
            client,
            data,
            channel,
        }
    }

    pub fn id(&self) -> u64 {
        self.data.id
    }

    pub async fn trace<D: Into<Bytes>>(&self, data: D) {
        let r = JobRequest::Trace(data.into());
        self.channel.send(r).await.unwrap();
    }

    pub fn variable(&self, key: &str) -> Option<Variable> {
        self.data.variables.get(key).map(|v| Variable { v })
    }

    pub fn dependencies(&self) -> impl Iterator<Item = Dependency> {
        self.data
            .dependencies
            .iter()
            .map(move |dependency| Dependency {
                job: self,
                dependency,
            })
    }
}
