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

        let token = if let Some(header) = request.headers.get(&"JOB-TOKEN".into()) {
            header[0].as_str()
        } else {
            return ResponseTemplate::new(403);
        };

        let (start, end) = if let Some(range) = request.headers.get(&"Content-Range".into()) {
            let mut split = range[0].as_str().splitn(2, '-');
            let start = split.next().unwrap().parse().unwrap();
            let end = split.next().unwrap().parse().unwrap();
            (start, end)
        } else {
            return ResponseTemplate::new(400);
        };

        if let Some(job) = self.mock.get_job(id) {
            if token != job.token() {
                ResponseTemplate::new(403)
            } else if job.state() != MockJobState::Running {
                ResponseTemplate::new(403).insert_header("Job-Status", &*job.state().to_string())
            } else {
                match job.append_log(request.body.clone(), start, end) {
                    Ok(()) => ResponseTemplate::new(202)
                        .insert_header(
                            "X-GitLab-Trace-Update-Interval",
                            &*self.mock.update_interval().to_string(),
                        )
                        .insert_header("Job-Status", &*job.state().to_string()),
                    Err(e) => ResponseTemplate::new(416).set_body_string(format!("{:?}", e)),
                }
            }
        } else {
            ResponseTemplate::new(404)
        }
    }
}
