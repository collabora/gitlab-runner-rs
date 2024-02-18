use http::StatusCode;
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

        if let Some(job) = self.mock.get_job(id) {
            if r.token != job.token() {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
            } else {
                let r = match (job.state(), r.state) {
                    (MockJobState::Running, MockJobState::Success) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(StatusCode::OK)
                    }
                    (MockJobState::Running, MockJobState::Failed) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(StatusCode::OK)
                    }
                    (MockJobState::Running, MockJobState::Running) => {
                        job.update_state(r.state);
                        ResponseTemplate::new(StatusCode::OK)
                    }
                    (current_state, _) if current_state != MockJobState::Running => {
                        ResponseTemplate::new(StatusCode::FORBIDDEN)
                    }
                    _ => panic!("Invalid state change"),
                };

                r.append_header("Job-Status", &*job.state().to_string())
            }
        } else {
            ResponseTemplate::new(StatusCode::NOT_FOUND)
        }
    }
}
