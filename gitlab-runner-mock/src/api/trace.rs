use http::StatusCode;
use wiremock::ResponseTemplate;
use wiremock::{Request, Respond};

use crate::{GitlabRunnerMock, MockJobState};

pub(crate) struct JobTraceResponder {
    mock: GitlabRunnerMock,
}

impl JobTraceResponder {
    pub(crate) fn new(mock: GitlabRunnerMock) -> Self {
        Self { mock }
    }
}

impl Respond for JobTraceResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        /* api/v4/jobs/<id>/trace */
        let id = request
            .url
            .path_segments()
            .unwrap()
            .nth_back(1)
            .unwrap()
            .parse()
            .unwrap();

        let token = if let Some(header) = request.headers.get("JOB-TOKEN") {
            header.to_str().expect("Invalid JOB-TOKEN value")
        } else {
            return ResponseTemplate::new(StatusCode::FORBIDDEN);
        };

        let (start, end) = if let Some(range) = request.headers.get("Content-Range") {
            let mut split = range
                .to_str()
                .expect("Invalid Content-Range value")
                .splitn(2, '-');
            let start = split.next().unwrap().parse().unwrap();
            let end = split.next().unwrap().parse().unwrap();
            (start, end)
        } else {
            return ResponseTemplate::new(StatusCode::BAD_REQUEST);
        };

        if let Some(job) = self.mock.get_job(id) {
            if token != job.token() {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
            } else if job.state() != MockJobState::Running {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
                    .insert_header("Job-Status", &*job.state().to_string())
            } else {
                match job.append_log(request.body.clone(), start, end) {
                    Ok(()) => ResponseTemplate::new(StatusCode::ACCEPTED)
                        .insert_header(
                            "X-GitLab-Trace-Update-Interval",
                            &*self.mock.update_interval().to_string(),
                        )
                        .insert_header("Job-Status", &*job.state().to_string()),
                    Err(e) => ResponseTemplate::new(StatusCode::RANGE_NOT_SATISFIABLE)
                        .set_body_string(format!("{e:?}")),
                }
            }
        } else {
            ResponseTemplate::new(StatusCode::NOT_FOUND)
        }
    }
}
