use http_types::StatusCode;
// Required for serde_json::json macro on the massive job json
use serde::Deserialize;
use serde_json::json;
use wiremock::ResponseTemplate;
use wiremock::{Request, Respond};

use crate::{GitlabRunnerMock, MockJobState};

/*
  jobs/id => 200  if ok;

  403 if cancelled
  < job-status: canceled in header
}
*/

#[derive(Default, Deserialize)]
struct FeaturesInfo {
    // This feature tells GitLab that the runner supports cloning from refspecs,
    // which is needed for the server to send jobs from run on detached MR
    // pipelines.
    #[serde(default)]
    refspecs: bool,
}

#[derive(Default, Deserialize)]
struct VersionInfo {
    #[serde(default)]
    features: FeaturesInfo,
}

#[derive(Deserialize)]
pub(crate) struct JobRequest {
    token: String,
    #[serde(default)]
    info: VersionInfo,
}

pub(crate) struct JobRequestResponder {
    mock: GitlabRunnerMock,
}

impl JobRequestResponder {
    pub fn new(mock: GitlabRunnerMock) -> Self {
        Self { mock }
    }
}

impl Respond for JobRequestResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let r: JobRequest = serde_json::from_slice(&request.body).unwrap();

        if r.token != self.mock.runner_token() {
            return ResponseTemplate::new(403);
        }

        if !r.info.features.refspecs {
            return ResponseTemplate::new(StatusCode::NoContent);
        }

        if let Some(job) = self.mock.grab_pending_job() {
            job.update_state(MockJobState::Running);
            let dependencies: Vec<_> = job
                .dependencies()
                .iter()
                .map(|j| {
                    let artifact = j.artifact();
                    let artifact_file = if !artifact.is_empty() {
                        Some(json!({
                            "filename": "artifacts.zip",
                            "size": artifact.len(),
                        }))
                    } else {
                        None
                    };
                    json!({
                        "id": j.id() ,
                        "name": j.name() ,
                        "token": j.token(),
                        "artifacts_file": artifact_file
                    })
                })
                .collect();
            let artifacts: Vec<_> = job
                .artifacts()
                .iter()
                .map(|a| {
                    let mut value = json!({
                        "name": a.name,
                        "paths": a.paths,
                        "when": a.when,
                        "exprire_in": a.expire_in,
                        "artifact_type": a.artifact_type,
                        "artifact_format": a.artifact_format,
                    });

                    // Missing values for 'untracked' should be treated as
                    // false, so we "test" that out here.
                    if a.untracked {
                        value
                            .as_object_mut()
                            .unwrap()
                            .insert("untracked".to_owned(), json!(a.untracked));
                    }

                    value
                })
                .collect();
            ResponseTemplate::new(201).set_body_json(json!({
                "id": job.id(),
                "token": job.token(),
                "allow_git_fetch": true,
                "job_info": {
                    "id": job.id(),
                    "name": job.name(),
                    "stage": "build",
                    "project_id": 4,
                    "project_name": "gitlab-test"
                },
                "git_info": {
                    "repo_url": "https://bla/dummy.git",
                    "ref": "custom",
                    "sha": "265c14cf140a66cfc61e40e4ab45c95ba8df5ed1",
                    "before_sha": "fc40ad32cdd36b814f07a540605110edc209a38c",
                    "ref_type": "branch",
                    "refspecs": [
                        "+265c14cf140a66cfc61e40e4ab45c95ba8df5ed1:refs/pipelines/120",
                        "+refs/heads/custom:refs/remotes/origin/custom"
                    ],
                    "depth": 50
                },
                "runner_info": {
                    "timeout": 3600,
                    "runner_session_url": null
                },
                "variables": job.variables(),
                "steps": job.steps(),
                "image": null,
                "services": [],
                "artifacts": artifacts,
                "cache": [],
                "credentials": [
                    {
                        "type": "registry",
                        "url": "registry.example.com",
                        "username": "gitlab-ci-token",
                        "password": "registry-dummy-tokn"
                    },
                    {
                        "type": "registry",
                        "url": "registry.example.com.nl:443",
                        "username": "gitlab-ci-token",
                        "password": "registry-dummy-tokn"
                    }
                ],
                "dependencies": dependencies,
                "features": {
                    "trace_sections": true,
                    "failure_reasons": [
                        "unknown_failure",
                        "script_failure",
                        "api_failure",
                        "stuck_or_timeout_failure",
                        "runner_system_failure",
                        "missing_dependency_failure",
                        "runner_unsupported",
                        "stale_schedule",
                        "job_execution_timeout",
                        "archived_failure",
                        "unmet_prerequisites",
                        "scheduler_failure",
                        "data_integrity_failure",
                        "forward_deployment_failure",
                        "user_blocked",
                        "project_deleted",
                        "insufficient_bridge_permissions",
                        "downstream_bridge_project_not_found",
                        "invalid_bridge_trigger",
                        "bridge_pipeline_is_child_pipeline",
                        "downstream_pipeline_creation_failed",
                        "secrets_provider_not_found",
                        "reached_max_descendant_pipelines_depth"
                    ]
                }
            }))
        } else {
            ResponseTemplate::new(StatusCode::NoContent)
        }
    }
}
