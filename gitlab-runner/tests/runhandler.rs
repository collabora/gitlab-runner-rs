use std::pin::Pin;

// Test the interaction intervals with the backend as driver by the runhandler
//
use gitlab_runner::job::Job;
use gitlab_runner::{CancelClient, JobHandler, JobResult, Phase, Runner};
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
    async fn step(
        &mut self,
        _script: &[String],
        _phase: Phase,
        _cancel: CancelClient,
    ) -> JobResult {
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

/// busy wait while the closure returns true
async fn busy_wait<F>(f: F)
where
    F: Fn() -> bool,
{
    while f() {
        // Use std lib (blocking) sleep to avoid the
        // tests time adjustments.
        std::thread::sleep(Duration::from_millis(5));
        tokio::task::yield_now().await;
    }
}

async fn busy_delay(d: Duration) {
    // Measure using std time instant to avoid tokios time adjustment
    let now = std::time::Instant::now();
    let deadline = now + d;
    busy_wait(|| deadline >= std::time::Instant::now()).await
}

async fn log_ping(job: &MockJob, control: &LoggerControl, mut patches: u32) -> u32 {
    let ping = "ping";
    control.log_wait(ping.to_string()).await;

    // Make a leap forward; The logging loop doesn't catch up with lost ticks so this syncs up the
    // interval again
    tokio::time::advance(Duration::from_secs(100)).await;

    // Busy wait for the log patches to be receive by the mock server
    busy_wait(|| job.log_patches() == patches).await;
    patches += 1;

    assert_eq!(job.log_patches(), patches);
    assert_eq!(job.log_last(), Some(ping.as_bytes().to_vec()));

    patches
}

/// Log a various lines and check if it got logged in one batch
/// This will flush the job buffer!
async fn log_batch(
    job: &MockJob,
    control: &LoggerControl,
    mut patches: u32,
    interval: Duration,
) -> u32 {
    // Send two seperate message and yield a bunch of times after each; Even when doing so neither
    // entry should be sent to the gitlab server just yet; Only when the time is advanced by the
    // interval it's expected to arrive..
    //
    // Do a sleep inside the busy wait loop to give the OS some time to do its processing in case
    // the client doesn't behave like expected
    control.log_wait("1".to_string()).await;
    tokio::time::advance(interval / 4).await;
    busy_delay(Duration::from_millis(100)).await;
    assert_eq!(job.log_patches(), patches, "Seen an unexpected log patch");

    control.log_wait("2".to_string()).await;
    tokio::time::advance(interval / 4).await;
    busy_delay(Duration::from_millis(100)).await;
    assert_eq!(job.log_patches(), patches, "Seen an unexpected log patch");

    // Jump time by interval; which should cause a single tick on the trace update interval
    tokio::time::advance(interval / 2).await;

    // Busy wait for the log patches to be receive by the mock server
    busy_wait(|| job.log_patches() == patches).await;
    patches += 1;

    assert_eq!(job.log_patches(), patches);
    assert_eq!(job.log_last(), Some(vec![b'1', b'2']));

    patches
}

// Test only set only every 3 seconds
// Test it gets sent on the rate requested
// Test Update is called regularily
#[tokio::test(start_paused = true)]
async fn update_interval() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("logging".to_string());

    let dir = tempfile::tempdir().unwrap();
    let mut runner = Runner::new(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let (control, rx) = LoggerControl::new();

    let got_job = runner
        .request_job(|job| async move { Ok(Logger::new(job, rx)) })
        .await
        .unwrap();
    assert!(got_job);

    let mut patches = 0;
    // Ping the logger which guarantees the job is fully started as well as getting the logging
    // loop back into a known point.
    patches = log_ping(&job, &control, patches).await;

    // Default interval is 3, so do a bit more seconds
    let interval = Duration::from_secs(4);
    patches = log_batch(&job, &control, patches, interval).await;
    patches = log_batch(&job, &control, patches, interval).await;
    patches = log_batch(&job, &control, patches, interval).await;

    // First state update was pending -> running
    assert_eq!(job.state_updates(), 1);

    // After 30 seconds of not sending logs a job update should be sent
    tokio::time::advance(Duration::from_secs(40)).await;
    busy_wait(|| job.state_updates() == 1).await;
    assert_eq!(job.state_updates(), 2);
    assert_eq!(job.log_patches(), patches);

    // Check one log attempt to make sure the intervals aren't trying to catch up
    patches = log_batch(&job, &control, patches, interval).await;

    // Update interval and then send another trace; the new trace will get the update interval
    mock.set_update_interval(30);
    patches = log_batch(&job, &control, patches, interval).await;

    let interval = Duration::from_secs(30);
    patches = log_batch(&job, &control, patches, interval).await;
    assert_eq!(job.log_patches(), patches);
    assert_eq!(job.state_updates(), 2);

    /* Trigger another keepalive by jumping more then the period and more then the default
     * keepalive of 30 seconds  */
    tokio::time::advance(Duration::from_secs(40)).await;
    busy_wait(|| job.state_updates() == 2).await;

    assert_eq!(job.state_updates(), 3);
    assert_eq!(job.log_patches(), patches);

    /* and back to normal */
    tokio::time::resume();
    control.finish(Ok(())).await;

    runner.wait_for_space(1).await;
    assert_eq!(MockJobState::Success, job.state());
}
