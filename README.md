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
        PathBuf::from("/tmp"));
    runner.run(move | _job | async move { Ok(Run{})  }, 16).await.unwrap();
}
```

## Gitlab runner registration

This crate does not support registering new runners with the gitlab server, so this has to be
done by hand using the gitlab
[runner registration API](https://docs.gitlab.com/ee/api/runners.html#register-a-new-runner).

The registration token can be retrieved from the runners section in the Gitlab
administration area. With that token the runner can be register using a curl
command like:
```shell
curl --request POST "https://GITLAB_URL/api/v4/runners"  \
  --form "description=My custom runner" \
  --form "run_untagged=false" \
  --form "tag_list=custom-gitlab-runner" \
  --form "token=REGISTRATION_TOKEN"
```

As a response to this command a new token for the registered runner will be
provided, this token should be provided to the runner for it's gitlab
connection.

One thing to key parameter provided here is `run_untagged=false`, which will
make the runner *only* pickup jobs which matches its tag. This is important to
prevent the runner from picking up "normal" jobs which it will not be able to
process.
