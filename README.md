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
use gitlab_runner::{outputln, Runner, JobHandler, JobResult, Phase};
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
    let mut runner = Runner::new(
        "https://gitlab.example.com".try_into().unwrap(),
        "runner token".to_owned(),
        "system id".to_owned(),
        PathBuf::from("/tmp"));
    runner.run(move | _job | async move { Ok(Run{})  }, 16).await.unwrap();
}
```

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
connection, along with a system ID that identifies the machine on which the
runner is executed.

The system ID should be a unique string. GitLab doesn't currently require any
particular formatting, but it is recommended to follow the way the official
`gitlab-runner` creates system IDs:

- Deriving it from the machine ID, found in `/var/lib/dbus/machine-id` or
  `/etc/machine-id`, but concatenating the machine ID with the string
  "gitlab-runner", taking the first 12 characters of its SHA256 hash in hex
  form, and prepending it with `s_`.

- Generating a random 12-character string using letters and digits
  (`[a-z][A-Z][0-9]`), and prepending it with `r_`.

In either case the system ID should be recorded in a persistent storage, along
with the authentication token, and be passed to the `Runner::new()` function.

The token can be verified using a curl command like:

```shell
curl --request POST "https://GITLAB_URL/api/v4/runners/verify"  \
  --form "token=AUTHENTICATION_TOKEN" \
  --form "system_id=SYSTEM_ID"
```

This step is optional. If performed, it will pre-register the system ID with
the GitLab server. Otherwise the system ID will be registered the first time
the runner pings for jobs.
