use futures::future;
use gitlab_runner::job::Job;
use gitlab_runner::{outputln, JobHandler, JobResult, Phase, Runner};
use gitlab_runner_mock::{
    GitlabRunnerMock, MockJob, MockJobState, MockJobStepName, MockJobStepWhen,
};
use std::fmt::Debug;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::Registry;

use std::time::Duration;
use tokio::sync::oneshot::{self, Receiver, Sender};
use tokio::time::sleep;

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

#[tokio::test]
async fn job_success() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("job success".to_string());

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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
async fn job_log() {
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("log".to_string());

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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

#[tokio::test(flavor = "multi_thread")]
async fn job_parallel() {
    let mock = GitlabRunnerMock::start().await;
    let jobs = TestJobs::new(mock.clone());
    for _ in 0..5 {
        jobs.add_job();
    }

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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
        assert!(testjobs
            .iter()
            .any(|j| j.job.state() == MockJobState::Running));

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
        assert!(testjobs
            .iter()
            .any(|j| j.job.state() == MockJobState::Success));
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

    let dir = tempfile::tempdir().unwrap();
    let (mut r, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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
        assert!(testjobs
            .iter()
            .all(|j| j.job.state() == MockJobState::Success));
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

    let dir = tempfile::tempdir().unwrap();
    let (mut r, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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

            assert!(running <= JOB_LIMIT, "running {} > {}", running, N_JOBS);
            if running == JOB_LIMIT || jobs.pending() == 0 {
                if let Some(j) = jobs.jobs().iter().find(|j| j.is_running()) {
                    if !j.completed() {
                        j.complete(()).await;
                    }
                }
            }

            if jobs.finished() == N_JOBS {
                break;
            }
        }

        // Should all have finished successfully now
        assert!(jobs
            .jobs()
            .iter()
            .all(|j| j.job.state() == MockJobState::Success));
    }
    .with_subscriber(subscriber)
    .await;
}

#[tokio::test]
async fn runner_delay() {
    //tokio::time::pause();
    let mock = GitlabRunnerMock::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut r, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
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
    let mock = GitlabRunnerMock::start().await;
    let job = mock.add_dummy_job("variables".to_string());

    let dir = tempfile::tempdir().unwrap();
    let (mut runner, layer) = Runner::new_with_layer(
        mock.uri(),
        mock.runner_token().to_string(),
        dir.path().to_path_buf(),
    );

    let subscriber = Registry::default().with(layer);
    async {
        let got_job = runner
            .request_job(|job| async move {
                let id = job.variable("CI_JOB_ID").unwrap();
                assert_eq!(job.id(), id.value().parse::<u64>().unwrap());
                assert!(id.public());
                assert!(!id.masked());
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
