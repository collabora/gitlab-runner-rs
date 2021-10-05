use serde::Deserialize;
use wiremock::ResponseTemplate;
use wiremock::{Request, Respond};

use crate::GitlabRunnerMock;
use crate::MockJobState;

#[derive(Debug, Deserialize)]
pub(crate) struct JobUpdate {
    token: String,
    state: MockJobState,
}

pub(crate) struct JobUpdateResponder {
    mock: GitlabRunnerMock,
}

impl JobUpdateResponder {
    pub(crate) fn new(mock: GitlabRunnerMock) -> Self {
        Self { mock }
    }
}

impl Respond for JobUpdateResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let r: JobUpdate = serde_json::from_slice(&request.body).unwrap();
        let id = request
            .url
            .path_segments()
            .unwrap()
            .last()
            .unwrap()
            .parse()
            .unwrap();

        if let Some(job) = self.mock.find_job(id) {
            if r.token != job.token() {
                ResponseTemplate::new(403)
            } else {
                match (job.state(), r.state) {
                    (MockJobState::Running, MockJobState::Success) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(200)
                    }
                    (MockJobState::Running, MockJobState::Failed) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(200)
                    }
                    (MockJobState::Running, MockJobState::Running) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(200)
                    }
                    _ => panic!("Invalid state change"),
                }
            }
        } else {
            ResponseTemplate::new(404)
        }
    }
}
