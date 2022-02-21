use http_types::headers::CONTENT_TYPE;
use http_types::StatusCode;
use multipart::server::Multipart;
use std::io::Read;
use wiremock::ResponseTemplate;
use wiremock::{Request, Respond};

use crate::GitlabRunnerMock;

pub(crate) struct JobArtifactsUploader {
    mock: GitlabRunnerMock,
}

impl JobArtifactsUploader {
    pub(crate) fn new(mock: GitlabRunnerMock) -> Self {
        Self { mock }
    }
}

impl Respond for JobArtifactsUploader {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        /* post api/v4/jobs/<id>/artifacts */
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

        if let Some(job) = self.mock.find_job(id) {
            if token != job.token() {
                ResponseTemplate::new(403)
            } else {
                let ct = request
                    .headers
                    .get(&CONTENT_TYPE)
                    .expect("Missing content type")
                    .get(0)
                    .expect("Empty header array?");

                let boundary = ct
                    .as_str()
                    .split_once("boundary=")
                    .map(|x| x.1)
                    .expect("Missing boundary");

                let mut multipart =
                    Multipart::with_body(std::io::Cursor::new(&request.body), boundary);
                while let Some(mut part) = multipart.read_entry().unwrap() {
                    if &*part.headers.name == "file" {
                        let mut data = Vec::new();
                        part.data
                            .read_to_end(&mut data)
                            .expect("failed to read multipart data");
                        job.set_artifact(data);
                    }
                }

                ResponseTemplate::new(StatusCode::Created)
            }
        } else {
            ResponseTemplate::new(404)
        }
    }
}

pub(crate) struct JobArtifactsDownloader {
    mock: GitlabRunnerMock,
}

impl JobArtifactsDownloader {
    pub(crate) fn new(mock: GitlabRunnerMock) -> Self {
        Self { mock }
    }
}

impl Respond for JobArtifactsDownloader {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        /* get api/v4/jobs/<id>/artifacts */
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
            return ResponseTemplate::new(StatusCode::Forbidden);
        };

        if let Some(job) = self.mock.find_job(id) {
            if token != job.token() {
                ResponseTemplate::new(StatusCode::Forbidden)
            } else {
                ResponseTemplate::new(StatusCode::Ok).set_body_bytes(job.artifact().as_slice())
            }
        } else {
            ResponseTemplate::new(StatusCode::NotFound)
        }
    }
}
