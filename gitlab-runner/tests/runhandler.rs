use std::pin::Pin;

// Test the interaction intervals with the backend as driver by the runhandler
//
use gitlab_runner::job::Job;
use gitlab_runner::{JobHandler, JobResult, Phase, Runner};
use gitlab_runner_mock::{GitlabRunnerMock, MockJob, MockJobState};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};

struct Completion(oneshot::Receiver<()>);

impl std::future::Future for Completion {
    type Output = Result<(), ()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        Pin::new(&mut this.0).poll(cx).map_err(|_e| ())
    }
}

#[derive(Debug)]
enum Control {
    Log(String, oneshot::Sender<()>),
    #[allow(dead_code)]
    Sleep(u64, oneshot::Sender<()>),
    Finish(JobResult),
}

struct LoggerControl(mpsc::Sender<Control>);

impl LoggerControl {
    fn new() -> (Self, mpsc::Receiver<Control>) {
        let (tx, rx) = mpsc::channel(8);
        (LoggerControl(tx), rx)
    }

    /// Request the running job to create a lot message without waitng for it
    async fn log(&self, msg: String) -> Completion {
        let (tx, rx) = oneshot::channel();
        let control = Control::Log(msg, tx);
        self.0.send(control).await.expect("Failed to send control");
        Completion(rx)
    }

    /// Create a log message and wait for completion
    async fn log_wait(&self, msg: String) {
        let completion = self.log(msg).await;
        completion.await.expect("Faild to wait for log completion");
    }

    #[allow(dead_code)]
    async fn sleep(&self, time: u64) -> Completion {
        let (tx, rx) = oneshot::channel();
        let control = Control::Sleep(time, tx);
        self.0.send(control).await.expect("Failed to send control");
        Completion(rx)
    }

    async fn finish(self, result: JobResult) {
        let control = Control::Finish(result);
        self.0.send(control).await.expect("Failed to send control");
    }
}

struct Logger {
    job: Job,
    rx: mpsc::Receiver<Control>,
}

impl Logger {
    fn new(job: Job, rx: mpsc::Receiver<Control>) -> Self {
        Self { job, rx }
    }
}

#[async_trait::async_trait]
impl JobHandler for Logger {
    async fn step(&mut self, _script: &[String], _phase: Phase) -> JobResult {
        while let Some(command) = self.rx.recv().await {
            match command {
                Control::Log(s, tx) => {
                    self.job.trace(s);
                    let _ = tx.send(());
                }
                Control::Sleep(d, tx) => {
                    sleep(Duration::from_secs(d)).await;
                    let _ = tx.send(());
                }
                Control::Finish(result) => return result,
            }
        }
        // Remote quit without telling the job to end
        Err(())
    }
}

async fn log_batch(
    job: &MockJob,
    control: &LoggerControl,
    mut patches: u32,
    expected: &mut Vec<u8>,
) -> u32 {
    control.log("1".to_string()).await;
    for _ in 0u32..2048 {
        tokio::task::yield_now().await;
    }
    control.log_wait("2".to_string()).await;
    for _ in 0u32..2048 {
        tokio::task::yield_now().await;
    }
    expected.extend(b"12");

    assert_eq!(job.log_patches(), patches);

    // Jump time by 4 seconds; which should cause a single ticket on the trace update interval
    tokio::time::advance(Duration::from_secs(4)).await;
    // Busy wait for the log patches to be receive by the mock server
    while job.log_patches() == patches {
        tokio::task::yield_now().await;
    }

    patches += 1;
    assert_eq!(job.log_patches(), patches);
    assert_eq!(job.log(), *expected);

    patches
}

// Test only set only every 3 seconds
// Test it gets sent on the rate requested
// Test Update is called regularily
#[tokio::test(start_paused = true)]
async fn update_interval() {
    let mut expected = Vec::new();
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("loging".to_string());

    let mut runner = Runner::new(mock.uri(), mock.runner_token().to_string());

    let (control, rx) = LoggerControl::new();

    let got_job = runner
        .request_job(|job| async move { Ok(Logger::new(job, rx)) })
        .await
        .unwrap();
    assert_eq!(true, got_job);

    let mut patches = log_batch(&job, &control, 0, &mut expected).await;
    patches = log_batch(&job, &control, patches, &mut expected).await;
    patches = log_batch(&job, &control, patches, &mut expected).await;

    // First state update was pendng -> running
    assert_eq!(job.state_updates(), 1);

    // After 30 secons of not sending logs a job update should be sent
    tokio::time::advance(Duration::from_secs(40)).await;
    while job.state_updates() == 1 {
        tokio::task::yield_now().await;
    }
    assert_eq!(job.state_updates(), 2);
    assert_eq!(job.log_patches(), patches);

    // Check one log attempt to make sure the intervals aren't trying to catch up
    patches = log_batch(&job, &control, patches, &mut expected).await;

    /* and back to normal */
    tokio::time::resume();
    control.finish(Ok(())).await;

    runner.wait_job().await;
    assert_eq!(MockJobState::Success, job.state());
}
