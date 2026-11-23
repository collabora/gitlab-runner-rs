use futures::future;
use gitlab_runner::job::Job;
use gitlab_runner::{GitlabLayer, JobHandler, JobResult, Phase, Runner, RunnerBuilder, outputln};
use gitlab_runner_mock::{
    GitlabRunnerMock, MockJob, MockJobState, MockJobStepName, MockJobStepWhen,
};
use std::fmt::Debug;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tracing::Subscriber;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

use std::time::Duration;
use tokio::sync::oneshot::{self, Receiver, Sender};
use tokio::time::sleep;

async fn log_until_cancel() {
    // Output repeatedly because calling /trace will also check the job status
    // to see if it's cancelled.
    loop {
        outputln!("logging!");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct SimpleRun<S> {
    step: Option<S>,
}

impl SimpleRun<future::Ready<JobResult>> {
    async fn dummy(result: JobResult) -> Result<SimpleRun<future::Ready<JobResult>>, ()> {
        Ok(Self {
            step: Some(future::ready(result)),
        })
    }
}

#[async_trait::async_trait]
impl<S> JobHandler for SimpleRun<S>
where
    S: Future<Output = JobResult> + Send,
{
    async fn step(&mut self, _steps: &[String], _phase: Phase) -> JobResult {
        self.step.take().expect("Can't handle multiple steps").await
    }
}

struct CustomSteps<S> {
    steps: S,
}

impl<S, R> CustomSteps<S>
where
    S: Fn(&[String]) -> R + Send + Sync,
    R: Future<Output = JobResult> + Send,
{
    fn new(steps: S) -> Self {
        Self { steps }
    }
}

#[async_trait::async_trait]
impl<S, R> JobHandler for CustomSteps<S>
where
    S: Fn(&[String]) -> R + Send + Sync,
    R: Future<Output = JobResult> + Send,
{
    async fn step(&mut self, steps: &[String], _phase: Phase) -> JobResult {
        (self.steps)(steps).await
    }
}

struct NotifyOnDrop {
    sender: Option<Sender<()>>,
}

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.sender.take().unwrap().send(()).unwrap();
    }
}
struct CancellableRunHandle {
    dropped: Option<Receiver<()>>,
    started: Option<Receiver<()>>,
}

impl CancellableRunHandle {
    async fn started(&mut self) {
        self.started.take().unwrap().await.unwrap();
    }

    async fn dropped(&mut self) {
        self.dropped.take().unwrap().await.unwrap();
    }
}

struct CancellableRun {
    dropped: Option<Sender<()>>,
    started: Option<Sender<()>>,
}

impl CancellableRun {
    fn new() -> (Self, CancellableRunHandle) {
        let (dropped_send, dropped_recv) = oneshot::channel();
        let (started_send, started_recv) = oneshot::channel();

        (
            Self {
                dropped: Some(dropped_send),
                started: Some(started_send),
            },
            CancellableRunHandle {
                dropped: Some(dropped_recv),
                started: Some(started_recv),
            },
        )
    }
}

#[async_trait::async_trait]
impl JobHandler for CancellableRun {
    async fn step(&mut self, _steps: &[String], _phase: Phase) -> JobResult {
        self.started.take().expect("restarted?").send(()).unwrap();
        let _notify_on_drop = NotifyOnDrop {
            sender: Some(self.dropped.take().expect("restarted?")),
        };
        log_until_cancel().await;
        Ok(())
    }
}

struct TestJobCompletion<T> {
    rx: Option<Receiver<T>>,
    tx: Option<Sender<T>>,
}

struct TestRun<T> {
    start: Option<Receiver<T>>,
    delay: Option<Duration>,
}

#[async_trait::async_trait]
impl<T: Send> JobHandler for TestRun<T> {
    async fn step(&mut self, _steps: &[String], _phase: Phase) -> JobResult {
        self.start.take().expect("restarted?").await.unwrap();
        if let Some(delay) = self.delay {
            sleep(delay).await;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct TestJob<T> {
    job: MockJob,
    complete: Arc<Mutex<TestJobCompletion<T>>>,
}

impl<T: Debug> TestJob<T> {
    async fn complete(&self, data: T) {
        let mut completion = self.complete.lock().unwrap();
        completion
            .tx
            .take()
            .expect("Already complete")
            .send(data)
            .expect("Failed to send completion");
    }

    fn completed(&self) -> bool {
        let completion = self.complete.lock().unwrap();
        completion.tx.is_none()
    }

    fn register(&mut self) -> Receiver<T> {
        let mut completion = self.complete.lock().unwrap();
        completion.rx.take().expect("Already registered")
    }

    fn new(mock: &GitlabRunnerMock) -> Self {
        let job = mock.add_dummy_job("test job".to_string());
        let (tx, rx) = oneshot::channel();
        let completion = TestJobCompletion {
            rx: Some(rx),
            tx: Some(tx),
        };
        let complete = Arc::new(Mutex::new(completion));
        Self { job, complete }
    }

    fn is_pending(&self) -> bool {
        self.job.state() == MockJobState::Pending
    }

    fn is_running(&self) -> bool {
        self.job.state() == MockJobState::Running
    }

    fn is_finished(&self) -> bool {
        self.job.state().finished()
    }
}

struct TestJobsInner<T: Debug> {
    jobs: Vec<TestJob<T>>,
    mock: GitlabRunnerMock,
}

#[derive(Clone)]
struct TestJobs<T: Debug> {
    inner: Arc<Mutex<TestJobsInner<T>>>,
}

impl<T: Debug + Clone> TestJobs<T> {
    fn new(mock: GitlabRunnerMock) -> TestJobs<T> {
        let inner = Arc::new(Mutex::new(TestJobsInner {
            jobs: Vec::new(),
            mock,
        }));
        Self { inner }
    }

    fn add_job(&self) {
        let mut inner = self.inner.lock().unwrap();
        let t = TestJob::new(&inner.mock);
        inner.jobs.push(t);
    }

    fn register(&self, job: &Job) -> TestRun<T> {
        let mut inner = self.inner.lock().unwrap();
        let j = inner
            .jobs
            .iter_mut()
            .find(|j| j.job.id() == job.id())
            .expect("Unexpected job");
        TestRun {
            start: Some(j.register()),
            delay: None,
        }
    }

    fn access_jobs<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&Vec<TestJob<T>>) -> R,
    {
        let inner = self.inner.lock().unwrap();
        f(&inner.jobs)
    }

    fn jobs(&self) -> Vec<TestJob<T>> {
        self.access_jobs(|jobs| jobs.to_vec())
    }

    fn running(&self) -> usize {
        self.access_jobs(|jobs| jobs.iter().filter(|j| j.is_running()).count())
    }

    fn pending(&self) -> usize {
        self.access_jobs(|jobs| jobs.iter().filter(|j| j.is_pending()).count())
    }

    fn finished(&self) -> usize {
        self.access_jobs(|jobs| jobs.iter().filter(|j| j.is_finished()).count())
    }
}

async fn setup_runner(mock: &GitlabRunnerMock) -> (Runner, impl Subscriber, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (layer, jobs) = GitlabLayer::new();
    let subscriber = Registry::default()
        .with(layer)
        .with(tracing_subscriber::fmt::layer());
    let runner = RunnerBuilder::new(mock.uri(), mock.runner_token(), dir.path(), jobs)
        .build()
        .await;
    (runner, subscriber, dir)
}

#[tokio::test]
async fn job_success() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("job success".to_string());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let got_job = runner
            .request_job(|_| SimpleRun::dummy(Ok(())))
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_metadata() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("job success".to_string());
    mock.set_expected_metadata_version("version-1");
    mock.set_expected_metadata_revision("revision-2");
    mock.set_expected_metadata_platform("platform-3");
    mock.set_expected_metadata_architecture("architecture-4");

    let dir = tempfile::tempdir().unwrap();
    let (layer, jobs) = GitlabLayer::new();
    let subscriber = Registry::default()
        .with(layer)
        .with(tracing_subscriber::fmt::layer());
    let mut runner = RunnerBuilder::new(mock.uri(), mock.runner_token(), dir.path(), jobs)
        .version("version-1")
        .revision("revision-2")
        .platform("platform-3")
        .architecture("architecture-4")
        .build()
        .await;

    async {
        let got_job = runner
            .request_job(|_| SimpleRun::dummy(Ok(())))
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_fail() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("fail".to_string());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let got_job = runner
            .request_job(|_job| SimpleRun::dummy(Err(())))
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Failed, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_panic() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("panic".to_string());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let got_job = runner
            .request_job(|_job| async {
                panic!("Test panic");
                #[allow(unreachable_code)]
                SimpleRun::dummy(Ok(())).await
            })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Failed, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_cancel_step() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("cancel steps".to_string());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    let (run, mut handle) = CancellableRun::new();
    async {
        let got_job = runner.request_job(|_job| future::ok(run)).await.unwrap();
        assert!(got_job);
        handle.started().await;

        job.cancel();

        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Cancelled, job.state());
        handle.dropped().await;
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_log() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("log".to_string());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        tracing::info!("TEST");
        let got_job = runner
            .request_job(|job| async move {
                tracing::info!("TEST1234");
                outputln!("aa");
                job.trace("bb\n");
                outputln!("cc");
                SimpleRun::dummy(Ok(())).await
            })
            .with_current_subscriber()
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, job.state());
        assert_eq!(b"aa\nbb\ncc\n", job.log().as_slice());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_steps() {
    let mock = GitlabRunnerMock::start().await;
    let mut builder = mock.job_builder("steps".to_string());

    builder.add_step(
        MockJobStepName::Script,
        vec!["command0".to_string(), "command1".to_string()],
        1234,
        MockJobStepWhen::OnSuccess,
        false,
    );

    let job = builder.build();
    mock.enqueue_job(job.clone());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let got_job = runner
            .request_job(|_ob| async move {
                Ok(CustomSteps::new(|steps| {
                    assert_eq!(&["command0", "command1"], steps);
                    future::ready(Ok(()))
                }))
            })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[cfg(feature = "commands")]
#[tokio::test]
async fn command_handler_adapter_dispatches_and_forwards_lifecycle() {
    use clap::{Parser, Subcommand};
    use gitlab_runner::UploadableFile;
    use gitlab_runner::commands::{CommandHandler, CommandHandlerAdapter, ParseCommandError};
    use std::borrow::Cow;

    #[derive(Parser)]
    struct Command {
        #[command(subcommand)]
        action: Action,
    }

    #[derive(Subcommand)]
    enum Action {
        Echo { text: String },
        Fail,
    }

    struct File;

    #[async_trait::async_trait]
    impl UploadableFile for File {
        type Data<'a> = &'a [u8];

        fn get_path(&self) -> Cow<'_, str> {
            "result.txt".into()
        }

        async fn get_data(&self) -> Result<Self::Data<'_>, ()> {
            Ok(b"result")
        }
    }

    struct Handler {
        job: Job,
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl CommandHandler<File> for Handler {
        type Command = Command;

        fn job(&self) -> &Job {
            &self.job
        }

        fn on_entry(&mut self, entry: &str, phase: Phase) {
            outputln!("> {}", entry);
            self.events
                .lock()
                .unwrap()
                .push(format!("entry:{phase:?}:{entry}"));
        }

        async fn command(&mut self, command: Command, phase: Phase) -> JobResult {
            let (action, result) = match command.action {
                Action::Echo { text } => (text, Ok(())),
                Action::Fail => ("fail".into(), Err(())),
            };
            outputln!("{}", action);
            self.events
                .lock()
                .unwrap()
                .push(format!("command:{phase:?}:{action}"));
            result
        }

        async fn parse_error(
            &mut self,
            _entry: &str,
            phase: Phase,
            error: ParseCommandError,
        ) -> JobResult {
            let help = matches!(error, ParseCommandError::DisplayHelp(_));
            outputln!("{}", error);
            self.events
                .lock()
                .unwrap()
                .push(format!("parse-error:{phase:?}:{help}"));
            if help { Ok(()) } else { Err(()) }
        }

        async fn get_uploadable_files(
            &mut self,
        ) -> Result<Box<dyn Iterator<Item = File> + Send>, ()> {
            self.events.lock().unwrap().push("artifacts".into());
            Ok(Box::new(std::iter::once(File)))
        }

        async fn cleanup(&mut self) {
            self.events.lock().unwrap().push("cleanup".into());
        }
    }

    let mock = GitlabRunnerMock::start().await;
    let mut builder = mock.job_builder("typed commands".to_string());
    builder.add_variable("NAME".into(), "Jane Doe".into(), true, false);
    builder.add_step(
        MockJobStepName::Script,
        vec![
            "echo $NAME".into(),
            "--help".into(),
            "".into(),
            "echo later".into(),
        ],
        1234,
        MockJobStepWhen::OnSuccess,
        false,
    );
    builder.add_step(
        MockJobStepName::AfterScript,
        vec!["echo after".into(), "unknown".into(), "echo skipped".into()],
        1234,
        MockJobStepWhen::OnSuccess,
        false,
    );
    builder.add_artifact_paths(vec!["result.txt".into()]);
    let job = builder.build();
    mock.enqueue_job(job.clone());

    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        runner
            .request_job({
                let events = events.clone();
                |job| async move { Ok(CommandHandlerAdapter::new(Handler { job, events })) }
            })
            .await
            .unwrap();
        runner.wait_for_space(1).await;
        assert_eq!(job.state(), MockJobState::Success);
        assert_eq!(job.uploaded_artifacts().count(), 1);
        let log = String::from_utf8(job.log()).unwrap();
        assert!(log.contains("Jane Doe"), "{log}");
        assert!(log.contains("> --help"), "{log}");
        assert!(log.contains("Usage:"), "{log}");
        assert!(log.contains("unrecognized subcommand"), "{log}");
        assert_eq!(
            *events.lock().unwrap(),
            [
                "entry:Script:echo $NAME",
                "command:Script:Jane Doe",
                "entry:Script:--help",
                "parse-error:Script:true",
                "entry:Script:",
                "entry:Script:echo later",
                "command:Script:later",
                "entry:AfterScript:echo after",
                "command:AfterScript:after",
                "entry:AfterScript:unknown",
                "parse-error:AfterScript:false",
                "artifacts",
                "cleanup",
            ]
        );

        let mut builder = mock.job_builder("command failure".into());
        builder.add_step(
            MockJobStepName::Script,
            vec!["fail".into(), "echo skipped".into()],
            1234,
            MockJobStepWhen::OnSuccess,
            false,
        );
        let failed = builder.build();
        mock.enqueue_job(failed.clone());
        events.lock().unwrap().clear();
        runner
            .request_job({
                let events = events.clone();
                |job| async move { Ok(CommandHandlerAdapter::new(Handler { job, events })) }
            })
            .await
            .unwrap();
        runner.wait_for_space(1).await;
        assert_eq!(failed.state(), MockJobState::Failed);
        assert_eq!(
            *events.lock().unwrap(),
            ["entry:Script:fail", "command:Script:fail", "cleanup"]
        );
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn job_parallel() {
    let mock = GitlabRunnerMock::start().await;
    let jobs = TestJobs::new(mock.clone());
    for _ in 0..5 {
        jobs.add_job();
    }

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        while runner
            .request_job({
                let jobs = jobs.clone();
                move |job| future::ready(Ok(jobs.register(&job)))
            })
            .await
            .unwrap()
        {}

        // Picked up 5 jobs
        assert_eq!(5, runner.running());

        let testjobs = jobs.jobs();
        // All are running now
        assert!(
            testjobs
                .iter()
                .any(|j| j.job.state() == MockJobState::Running)
        );

        // Complete in random order
        for n in &[4, 0, 2, 1, 3] {
            let t = &testjobs[*n];
            assert_eq!(MockJobState::Running, t.job.state());

            let running = runner.running();
            t.complete(()).await;
            runner.wait_for_space(running).await;
            assert_eq!(MockJobState::Success, t.job.state());
        }

        assert_eq!(runner.running(), 0);

        // Should all have finished successfully now
        assert!(
            testjobs
                .iter()
                .any(|j| j.job.state() == MockJobState::Success)
        );
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_run() {
    let mock = GitlabRunnerMock::start().await;
    let jobs = TestJobs::new(mock.clone());
    for _ in 0..5 {
        jobs.add_job();
    }

    let (mut r, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let mut runner = tokio::task::spawn({
            let jobs = jobs.clone();
            async move {
                r.run(move |job| future::ready(Ok(jobs.register(&job))), 5)
                    .await
            }
        });

        let testjobs = jobs.jobs();
        // Complete in random order
        for n in &[4, 0, 2, 1, 3] {
            let t = &testjobs[*n];
            //assert_eq!(MockJobState::Running, t.job.state());
            while t.job.state() == MockJobState::Pending {
                if futures::poll!(&mut runner).is_ready() {
                    panic!("runner exited")
                }
                tokio::task::yield_now().await
            }

            t.complete(()).await;
            // busy wait till the job state moves to success
            while !t.job.finished() {
                if futures::poll!(&mut runner).is_ready() {
                    panic!("runner exited")
                }
                tokio::task::yield_now().await
            }
        }

        // Should all have finished successfully now
        assert!(
            testjobs
                .iter()
                .all(|j| j.job.state() == MockJobState::Success)
        );
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn runner_limit() {
    //tokio::time::pause();
    const JOB_LIMIT: usize = 3;
    const N_JOBS: usize = 32;
    let mock = GitlabRunnerMock::start().await;
    let jobs = TestJobs::new(mock.clone());
    for _ in 0..N_JOBS {
        jobs.add_job();
    }

    let (mut r, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let mut runner = tokio::task::spawn({
            let jobs = jobs.clone();
            async move {
                r.run(
                    {
                        move |job| {
                            let mut run = jobs.register(&job);
                            run.delay = Some(Duration::from_millis(33));
                            future::ready(Ok(run))
                        }
                    },
                    JOB_LIMIT,
                )
                .await
            }
        });

        loop {
            if futures::poll!(&mut runner).is_ready() {
                panic!("runner exited")
            }
            let running = jobs.running();

            assert!(running <= JOB_LIMIT, "running {running} > {N_JOBS}");
            if (running == JOB_LIMIT || jobs.pending() == 0)
                && let Some(j) = jobs.jobs().iter().find(|j| j.is_running())
                && !j.completed()
            {
                j.complete(()).await;
            }

            if jobs.finished() == N_JOBS {
                break;
            }
        }

        // Should all have finished successfully now
        assert!(
            jobs.jobs()
                .iter()
                .all(|j| j.job.state() == MockJobState::Success)
        );
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn runner_delay() {
    //tokio::time::pause();
    let mock = GitlabRunnerMock::start().await;

    let (mut r, subscriber, _dir) = setup_runner(&mock).await;
    async {
        tokio::task::spawn(async move { r.run(|_| SimpleRun::dummy(Ok(())), 1).await });

        // Give the runner some time to do a first request and go to sleep
        sleep(Duration::from_millis(250)).await;
        assert_eq!(1, mock.n_requests().await);

        // Jump ahead for longer then the expected sleep
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::time::resume();

        sleep(Duration::from_millis(250)).await;
        assert_eq!(2, mock.n_requests().await);

        mock.add_dummy_job("delayed job".to_string());
        // Jump ahead for longer then the expected sleep
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::time::resume();

        // 1 request for recieving the job, one to see ther isn't another one
        sleep(Duration::from_millis(250)).await;
        assert_eq!(4, mock.n_requests().await);

        // Jump ahead for longer then the expected sleep
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(7)).await;
        tokio::time::resume();

        sleep(Duration::from_millis(250)).await;
        assert_eq!(5, mock.n_requests().await);
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_variables() {
    const TEST_VARIABLE: &str = "TEST_VARIABLE";
    const TEST_VALUE: &str = "a testing value";

    let mock = GitlabRunnerMock::start().await;
    let mut builder = mock.job_builder("variables".to_string());

    builder.add_step(
        MockJobStepName::Script,
        vec!["dummy".to_owned()],
        3600,
        MockJobStepWhen::OnSuccess,
        false,
    );
    builder.add_variable(TEST_VARIABLE.to_owned(), TEST_VALUE.to_owned(), false, true);

    let job = builder.build();
    mock.enqueue_job(job.clone());

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        let got_job = runner
            .request_job(|job| async move {
                let id = job.variable("CI_JOB_ID").unwrap();
                assert_eq!(job.id(), id.value().parse::<u64>().unwrap());
                assert!(id.public());
                assert!(!id.masked());

                let test = job.variable(TEST_VARIABLE).unwrap();
                assert_eq!(test.value(), TEST_VALUE);
                assert!(!test.public());
                assert!(test.masked());

                SimpleRun::dummy(Ok(())).await
            })
            .await
            .unwrap();
        assert!(got_job);
        runner.wait_for_space(1).await;
        assert_eq!(MockJobState::Success, job.state());
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn job_drain() {
    let mock = GitlabRunnerMock::start().await;
    let jobs = TestJobs::new(mock.clone());
    for _ in 0..5 {
        jobs.add_job();
    }

    let (mut runner, subscriber, _dir) = setup_runner(&mock).await;
    async {
        while runner
            .request_job({
                let jobs = jobs.clone();
                move |job| future::ready(Ok(jobs.register(&job)))
            })
            .await
            .unwrap()
        {}

        // Picked up 5 jobs
        assert_eq!(5, runner.running());
        let testjobs = jobs.jobs();
        for n in &[4, 0, 2, 1, 3] {
            let t = &testjobs[*n];
            assert_eq!(MockJobState::Running, t.job.state());

            t.complete(()).await;
        }

        runner.drain().await;
        assert_eq!(0, runner.running());
    }
    .with_subscriber(subscriber)
    .await;
}
