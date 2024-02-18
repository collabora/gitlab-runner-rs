# gitlab-runner-rs

A crate to help build custom gitlab runner implementations.

## Implementing a custom runner

The overall idea is that this crate handles all interaction with the gitlab
server and drives the executions while the the runner implementation focus on
how to handle jobs from gitlab.

As the focus for an implementer of a custom runner is to implement the async
JobHandler trait, which gets calle during job executation. An absolute minimal
runner can be implement as such:

```rust,no_run
use gitlab_runner::{outputln, GitlabLayer, RunnerBuilder, JobHandler, JobResult, Phase};
use tracing_subscriber::prelude::*;
use std::path::PathBuf;

#[derive(Debug)]
struct Run {}

#[async_trait::async_trait]
impl JobHandler for Run {
      async fn step(&mut self, script: &[String], phase: Phase) -> JobResult {
          outputln!("Running script for phase {:?}", phase);
          for s in script {
            outputln!("Step: {}", s);
          }
          Ok(())
      }
}

#[tokio::main]
async fn main() {
    let (layer, jobs) = GitlabLayer::new();

    // By default show logs from my_runner and warnings, the gitlab_runner::gitlab::job span which
    // sets the gitlab.job field is always enabled regardless of the over all log level.
    let env = std::env::var("RUNNER_LOG").unwrap_or_else(|_| "my_runner=info,warn".to_string());
    let envfilter = tracing_subscriber::EnvFilter::builder()
        .parse(env)
        .unwrap_or_else(|| panic!("Failed to parse RUNNER_LOG env var: {env}"))
        .add_directive("gitlab_runner::gitlab::job=error".parse().unwrap());

    tracing_subscriber::Registry::default()
        .with(
            tracing_subscriber::fmt::Layer::new()
               .pretty()
               .with_filter(envfilter),
        )
        .with(layer)
        .init();
    let mut runner = RunnerBuilder::new(
        "https://gitlab.example.com".try_into().expect("failed to parse url"),
        "runner token",
        "/tmp",
        jobs
    )
    .build()
    .await;
    runner.run(move | _job | async move { Ok(Run{}) }, 16).await.unwrap();
}
```

When setting up the tracing subscriber only per-layer filtering should be used,
to ensure logs intended for gitlab don't get filtered out. For other logging
outputs the `gitlab_runner::gitlab::job` target should always be enabled to
get the gitlab.job field which contains the gitlab job id.

## Gitlab runner creation

This crate does not support creating new runners on the GitLab server. This can
be done using the
[runner creation API](https://docs.gitlab.com/ee/api/users.html#create-a-runner-linked-to-a-user),
or manually in the GitLab
[runner management web interface](https://docs.gitlab.com/ee/ci/runners/runners_scope.html).
Make sure to follow the runner creation with an authentication token workflow,
as the registration token workflow is deprecated.

One key parameter provided when creating the runner is `run_untagged=false` (or
leaving the `Run untagged jobs` box unchecked in the web interface), which will
make the runner *only* pickup jobs which matches its tags. This is important to
prevent the runner from picking up "normal" jobs which it will not be able to
process.

When the runner is created GitLab provides an authentication token starting
with `glrt-`. This token should be provided to the runner for its GitLab
connection.

The token can be verified using a curl command like:

```shell
curl --request POST "https://GITLAB_URL/api/v4/runners/verify"  \
  --form "token=AUTHENTICATION_TOKEN"
```

This step is optional.
