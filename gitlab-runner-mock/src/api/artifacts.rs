use http::header::CONTENT_TYPE;
use http::StatusCode;
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

        let token = if let Some(header) = request.headers.get("JOB-TOKEN") {
            header.to_str().expect("Invalid job token header value")
        } else {
            return ResponseTemplate::new(StatusCode::FORBIDDEN);
        };

        if let Some(job) = self.mock.get_job(id) {
            if token != job.token() {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
            } else {
                let ct = request
                    .headers
                    .get(&CONTENT_TYPE)
                    .expect("Missing content type");

                let boundary = ct
                    .to_str()
                    .expect("Invalid content type header value")
                    .split_once("boundary=")
                    .map(|x| x.1)
                    .expect("Missing boundary");

                let mut multipart =
                    Multipart::with_body(std::io::Cursor::new(&request.body), boundary);

                let mut filename = None;
                let mut data = Vec::new();
                let mut artifact_type = None;
                let mut artifact_format = None;
                while let Some(mut part) = multipart.read_entry().unwrap() {
                    match &*part.headers.name {
                        "file" => {
                            filename = part.headers.filename.clone();
                            part.data
                                .read_to_end(&mut data)
                                .expect("failed to read multipart data");
                        }
                        "artifact_format" => {
                            let mut value = String::new();
                            part.data
                                .read_to_string(&mut value)
                                .expect("failed to read artifact format");
                            artifact_format = Some(value);
                        }
                        "artifact_type" => {
                            let mut value = String::new();
                            part.data
                                .read_to_string(&mut value)
                                .expect("failed to read artifact type");
                            artifact_type = Some(value);
                        }
                        _ => {
                            unimplemented!("Unknown field in request: {}", &*part.headers.name);
                        }
                    }
                }
                job.add_artifact(
                    filename,
                    data,
                    artifact_type.as_deref(),
                    artifact_format.as_deref(),
                );

                ResponseTemplate::new(StatusCode::CREATED)
            }
        } else {
            ResponseTemplate::new(StatusCode::NOT_FOUND)
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

        let token = if let Some(header) = request.headers.get("JOB-TOKEN") {
            header.to_str().expect("Invalid JOB-TOKEN value")
        } else {
            return ResponseTemplate::new(StatusCode::FORBIDDEN);
        };

        if let Some(job) = self.mock.get_job(id) {
            if token != job.token() {
                ResponseTemplate::new(StatusCode::FORBIDDEN)
            } else {
                match job
                    .uploaded_artifacts()
                    .find(|a| a.artifact_type.as_deref() == Some("archive"))
                    .map(|a| a.data)
                {
                    Some(data) => {
                        ResponseTemplate::new(StatusCode::OK).set_body_bytes(data.as_slice())
                    }
                    None => ResponseTemplate::new(StatusCode::NOT_FOUND),
                }
            }
        } else {
            ResponseTemplate::new(StatusCode::NOT_FOUND)
        }
    }
}
