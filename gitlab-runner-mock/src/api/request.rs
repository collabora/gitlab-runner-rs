// Required for serde_json::json macro on the massive job json
use serde::Deserialize;
use serde_json::json;
use wiremock::ResponseTemplate;
use wiremock::{Request, Respond};

use crate::GitlabRunnerMock;

/*
  jobs/id => 200  if ok;

  403 if cancelled
  < job-status: canceled in header
}
*/

#[derive(Deserialize)]
pub(crate) struct JobRequest {
    token: String,
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

        if let Some(job) = self.mock.grab_pending_job() {
            let variables = json!([
              { "key": "CI_PIPELINE_ID", "value": "120", "public": true, "masked": false },
              { "key": "CI_PIPELINE_URL", "value": "https://gitlab.example.com/test/gitlab-test/-/pipelines/120", "public": true, "masked": false },
              { "key": "CI_JOB_ID", "value": job.id().to_string(), "public": true, "masked": false },
              { "key": "CI_JOB_URL", "value": "https://gitlab.example.com/test/gitlab-test/-/jobs/265", "public": true, "masked": false },
              { "key": "CI_JOB_TOKEN", "value": "tokn", "public": false, "masked": true },
              { "key": "CI_JOB_STARTED_AT", "value": "2021-05-05T17:16:32Z", "public": true, "masked": false },
              { "key": "CI_BUILD_ID", "value": "265", "public": true, "masked": false },
              { "key": "CI_BUILD_TOKEN", "value": "tokn", "public": false, "masked": true },
              { "key": "CI_REGISTRY_USER", "value": "gitlab-ci-token", "public": true, "masked": false },
              { "key": "CI_REGISTRY_PASSWORD", "value": "token", "public": false, "masked": true },
              { "key": "CI_REPOSITORY_URL", "value": "https://gitlab-ci-token:token@gitlab.example.com/test/gitlab-test.git", "public": false, "masked": false },
              { "key": "CI_DEPENDENCY_PROXY_USER", "value": "gitlab-ci-token", "public": true, "masked": false },
              { "key": "CI_DEPENDENCY_PROXY_PASSWORD", "value": "token", "public": false, "masked": true },
              { "key": "CI_JOB_NAME", "value": "custom", "public": true, "masked": false },
              { "key": "CI_JOB_STAGE", "value": "build", "public": true, "masked": false },
              { "key": "CI_NODE_TOTAL", "value": "1", "public": true, "masked": false },
              { "key": "CI_BUILD_NAME", "value": "custom", "public": true, "masked": false },
              { "key": "CI_BUILD_STAGE", "value": "build", "public": true, "masked": false },
              { "key": "GITLAB_CI", "value": "true", "public": true, "masked": false },
              { "key": "CI_SERVER_URL", "value": "https://gitlab.example.com", "public": true, "masked": false },
              { "key": "CI_SERVER_HOST", "value": "gitlab.example.com", "public": true, "masked": false },
              { "key": "CI_SERVER_PORT", "value": "443", "public": true, "masked": false },
              { "key": "CI_SERVER_PROTOCOL", "value": "https", "public": true, "masked": false },
              { "key": "CI_SERVER_NAME", "value": "GitLab", "public": true, "masked": false },
              { "key": "CI_SERVER_VERSION", "value": "13.11.3", "public": true, "masked": false },
              { "key": "CI_SERVER_VERSION_MAJOR", "value": "13", "public": true, "masked": false },
              { "key": "CI_SERVER_VERSION_MINOR", "value": "11", "public": true, "masked": false },
              { "key": "CI_SERVER_VERSION_PATCH", "value": "3", "public": true, "masked": false },
              { "key": "CI_SERVER_REVISION", "value": "b321336e443", "public": true, "masked": false },
              { "key": "GITLAB_FEATURES", "value": "", "public": true, "masked": false },
              { "key": "CI_PROJECT_ID", "value": "4", "public": true, "masked": false },
              { "key": "CI_PROJECT_NAME", "value": "gitlab-test", "public": true, "masked": false },
              { "key": "CI_PROJECT_TITLE", "value": "gitlab-test", "public": true, "masked": false },
              { "key": "CI_PROJECT_PATH", "value": "test/gitlab-test", "public": true, "masked": false },
              { "key": "CI_PROJECT_PATH_SLUG", "value": "test-gitlab-test", "public": true, "masked": false },
              { "key": "CI_PROJECT_NAMESPACE", "value": "test", "public": true, "masked": false },
              { "key": "CI_PROJECT_ROOT_NAMESPACE", "value": "test", "public": true, "masked": false },
              { "key": "CI_PROJECT_URL", "value": "https://gitlab.example.com/test/gitlab-test", "public": true, "masked": false },
              { "key": "CI_PROJECT_VISIBILITY", "value": "private", "public": true, "masked": false },
              { "key": "CI_PROJECT_REPOSITORY_LANGUAGES", "value": "dockerfile,ocaml", "public": true, "masked": false },
              { "key": "CI_DEFAULT_BRANCH", "value": "master", "public": true, "masked": false },
              { "key": "CI_PROJECT_CONFIG_PATH", "value": ".gitlab-ci.yml", "public": true, "masked": false },
              { "key": "CI_CONFIG_PATH", "value": ".gitlab-ci.yml", "public": true, "masked": false },
              { "key": "CI_PAGES_DOMAIN", "value": "example.com", "public": true, "masked": false },
              { "key": "CI_PAGES_URL", "value": "http://test.example.com/gitlab-test", "public": true, "masked": false }, { "key": "CI_REGISTRY", "value": "registry.example.com", "public": true, "masked": false },
              { "key": "CI_REGISTRY_IMAGE", "value": "registry.example.com/test/gitlab-test", "public": true, "masked": false },
              { "key": "CI_DEPENDENCY_PROXY_SERVER", "value": "gitlab.example.com:443", "public": true, "masked": false },
              { "key": "CI_DEPENDENCY_PROXY_GROUP_IMAGE_PREFIX", "value": "gitlab.example.com:443/test/dependency_proxy/containers", "public": true, "masked": false },
              { "key": "CI_API_V4_URL", "value": "https://gitlab.example.com/api/v4", "public": true, "masked": false },
              { "key": "CI_PIPELINE_IID", "value": "33", "public": true, "masked": false },
              { "key": "CI_PIPELINE_SOURCE", "value": "push", "public": true, "masked": false },
              { "key": "CI_PIPELINE_CREATED_AT", "value": "2021-05-05T10:20:00Z", "public": true, "masked": false },
              { "key": "CI_COMMIT_SHA", "value": "265c14cf140a66cfc61e40e4ab45c95ba8df5ed1", "public": true, "masked": false },
              { "key": "CI_COMMIT_SHORT_SHA", "value": "265c14cf", "public": true, "masked": false },
              { "key": "CI_COMMIT_BEFORE_SHA", "value": "fc40ad32cdd36b814f07a540605110edc209a38c", "public": true, "masked": false },
              { "key": "CI_COMMIT_REF_NAME", "value": "custom", "public": true, "masked": false },
              { "key": "CI_COMMIT_REF_SLUG", "value": "custom", "public": true, "masked": false },
              { "key": "CI_COMMIT_BRANCH", "value": "custom", "public": true, "masked": false },
              { "key": "CI_COMMIT_MESSAGE", "value": "test rust runner\n", "public": true, "masked": false },
              { "key": "CI_COMMIT_TITLE", "value": "test rust runner", "public": true, "masked": false },
              { "key": "CI_COMMIT_DESCRIPTION", "value": "", "public": true, "masked": false },
              { "key": "CI_COMMIT_REF_PROTECTED", "value": "false", "public": true, "masked": false },
              { "key": "CI_COMMIT_TIMESTAMP", "value": "2021-05-05T12:19:53+02:00", "public": true, "masked": false },
              { "key": "CI_COMMIT_AUTHOR", "value": "Test User <test@example.com>", "public": true, "masked": false },
              { "key": "CI_BUILD_REF", "value": "265c14cf140a66cfc61e40e4ab45c95ba8df5ed1", "public": true, "masked": false },
              { "key": "CI_BUILD_BEFORE_SHA", "value": "fc40ad32cdd36b814f07a540605110edc209a38c", "public": true, "masked": false },
              { "key": "CI_BUILD_REF_NAME", "value": "custom", "public": true, "masked": false },
              { "key": "CI_BUILD_REF_SLUG", "value": "custom", "public": true, "masked": false },
              { "key": "CI_RUNNER_ID", "value": "8", "public": true, "masked": false },
              { "key": "CI_RUNNER_DESCRIPTION", "value": "Rust runner test", "public": true, "masked": false },
              { "key": "CI_RUNNER_TAGS", "value": "rust-runner", "public": true, "masked": false },
              { "key": "GITLAB_USER_ID", "value": "2", "public": true, "masked": false },
              { "key": "GITLAB_USER_EMAIL", "value": "test@example.com", "public": true, "masked": false },
              { "key": "GITLAB_USER_LOGIN", "value": "test", "public": true, "masked": false },
              { "key": "GITLAB_USER_NAME", "value": "Test User", "public": true, "masked": false }
            ]);
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
                    json!({
                        "name": a.name,
                        "untracked": a.untracked,
                        "paths": a.paths,
                        "when": a.when,
                        "exprire_in": a.expire_in,
                        "artifact_type": a.artifact_type,
                        "artifact_format": a.artifact_format,
                    })
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
                "variables": variables,
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
            ResponseTemplate::new(204)
        }
    }
}
