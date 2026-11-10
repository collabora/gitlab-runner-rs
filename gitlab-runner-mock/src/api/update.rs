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
            .next_back()
            .unwrap()
            .parse()
            .unwrap();

        if let Some(job) = self.mock.get_job(id) {
            if r.token != job.token() || job.state() != MockJobState::Running {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
            } else {
                assert!(
                    matches!(
                        r.state,
                        MockJobState::Success | MockJobState::Failed | MockJobState::Running
                    ),
                    "Invalid state change from {} -> {}",
                    job.state(),
                    r.state,
                );

                job.update_state(r.state);
                ResponseTemplate::new(StatusCode::OK)
                    .append_header("Job-Status", job.state().to_string())
            }
        } else {
            ResponseTemplate::new(StatusCode::NOT_FOUND)
        }
    }
}
