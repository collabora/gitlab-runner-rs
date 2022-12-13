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

        if let Some(job) = self.mock.get_job(id) {
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

        if let Some(job) = self.mock.get_job(id) {
            if token != job.token() {
                ResponseTemplate::new(StatusCode::Forbidden)
            } else {
                match job
                    .uploaded_artifacts()
                    .find(|a| a.artifact_type.as_deref() == Some("archive"))
                    .map(|a| a.data)
                {
                    Some(data) => {
                        ResponseTemplate::new(StatusCode::Ok).set_body_bytes(data.as_slice())
                    }
                    None => ResponseTemplate::new(StatusCode::NotFound),
                }
            }
        } else {
            ResponseTemplate::new(StatusCode::NotFound)
        }
    }
}
