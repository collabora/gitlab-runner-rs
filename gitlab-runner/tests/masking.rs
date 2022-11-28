use futures::AsyncWriteExt;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{outputln, JobHandler, JobResult, Phase, Runner};
use gitlab_runner_mock::{GitlabRunnerMock, MockJobState, MockJobStepName, MockJobStepWhen};
use std::io::{Cursor, Read};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::Registry;
use zip::ZipArchive;

struct MaskTest {
    log_text: String,
}

#[async_trait::async_trait]
impl JobHandler for MaskTest {
    async fn step(&mut self, _script: &[String], _phase: Phase) -> JobResult {
        outputln!("{}", self.log_text.split('|').collect::<Vec<_>>().join(""));
        Ok(())
    }

    async fn upload_artifacts(&mut self, uploader: &mut Uploader) -> JobResult {
        let mut f = uploader.masked_file("masked".to_string()).await;
        f.write_all(
            format!(
                "{}\n",
                self.log_text.split('|').collect::<Vec<_>>().join("")
            )
            .as_bytes(),
        )
        .await
        .expect("Couldn't write test data");
        drop(f);
        let mut f = uploader.file("baseline".to_string()).await;
        f.write_all(self.log_text.as_bytes())
            .await
            .expect("Couldn't write test data");

        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct TestCase {
    prefixes: Vec<String>,
    input: String,
    expected: String,
    name: String,
}

macro_rules! emit_prefixes {
    ( $( $pfx:expr ),* ) => {
        vec![
            $(
                $pfx.to_string()
            ),*
        ]
    };
}

macro_rules! parse_test_case {
    ( $data:expr, ) => {};

    ( $data:expr, $name:literal : { input: $input:expr, expected: $expected:expr, } ) => {
        $data.push( TestCase { prefixes : Vec::new(), input: $input.to_string(), expected: format!("{}\n", $expected), name: $name.to_string() } );
    };

    ( $data:expr, $name:literal : { input: $input:expr, expected: $expected:expr, }, $($tail:tt)+ ) => {
        $data.push( TestCase { prefixes : Vec::new(), input: $input.to_string(), expected: format!("{}\n", $expected), name: $name.to_string() } );
        parse_test_case![$data, $($tail)*]
    };

    ( $data:expr, $name:literal : { prefixes: []string{ $($pfx:expr),* }, input: $input:expr, expected: $expected:expr, } ) => {
        $data.push( TestCase { prefixes : emit_prefixes![$($pfx),*], input: $input.to_string(), expected: format!("{}\n", $expected), name: $name.to_string() } );
    };

    ( $data:expr, $name:literal : { prefixes: []string{ $($pfx:expr),* }, input: $input:expr, expected: $expected:expr, }, $($tail:tt)* ) => {
        $data.push( TestCase { prefixes : emit_prefixes![$($pfx),*], input: $input.to_string(), expected: format!("{}\n", $expected), name: $name.to_string() } );
        parse_test_case![$data, $($tail)*]
    };
}

macro_rules! test_cases {
    () => {};

    ( $($tail:tt)* ) => {
        {
            let mut data = Vec::new();
            parse_test_case!(data, $($tail)+);
            data
        }
    };
}

#[tokio::test]
async fn mask_test() {
    // These test data are taken directly from the official
    // gitlab-runner source, and are still formatted for Go. We use a
    // simple pair of macros to convert them into an appropriate Rust
    // data structure.
    let test_cases = test_cases![
    "simple prefix masking": {
        input:    "Lorem ipsum dolor sit amet, ex ea commodo glpat-imperdiet in voluptate velit esse",
        expected: "Lorem ipsum dolor sit amet, ex ea commodo glpat-[MASKED] in voluptate velit esse",
    },
    "prefix at the end of the line": {
        input:    "Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esseglpat-imperdiet",
        expected: "Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esseglpat-[MASKED]",
    },
    "prefix at the beginning of the line": {
        input:    "glpat-imperdiet Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esse",
        expected: "glpat-[MASKED] Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esse",
    },
    "prefix inside of the line": {
        input:    "esseglpat-imperdiet=_-. end Lorem ipsum dolor sit amet, ex ea commodo  in voluptate velit",
        expected: "esseglpat-[MASKED] end Lorem ipsum dolor sit amet, ex ea commodo  in voluptate velit",
    },
    "two prefix concatenate": {
        input:    "glpat-impglpat-erdiet Lorem ipsum dolor sit amet, ex ea commodo  in voluptate velit esse",
        expected: "glpat-[MASKED] Lorem ipsum dolor sit amet, ex ea commodo  in voluptate velit esse",
    },
    "multiple packets pat masking": {
        input:    "glpat|-imperdiet Lorem ipsum dolor sit amet, ex ea commodo gl|pat-imperdiet in voluptate velit esse",
        expected: "glpat-[MASKED] Lorem ipsum dolor sit amet, ex ea commodo glpat-[MASKED] in voluptate velit esse",
    },
    "second multiple packets pat masking": {
        input:    "glpat| -imperdiet Lorem ipsum dolor sit amet",
        expected: "glpat -imperdiet Lorem ipsum dolor sit amet",
    },
    "long input": {
        input:    "Lorglpat-ipsu dolor sit amglpat-t, consglpat-ctglpat-tur adipiscing glpat-lit, sglpat-d do glpat-iusmod tglpat-mpor incididunt ut laborglpat-=_ glpat-t dolorglpat-=_ magna aliqua.",
        expected: "Lorglpat-[MASKED] dolor sit amglpat-[MASKED], consglpat-[MASKED] adipiscing glpat-[MASKED], sglpat-[MASKED] do glpat-[MASKED] tglpat-[MASKED] incididunt ut laborglpat-[MASKED] glpat-[MASKED] dolorglpat-[MASKED] magna aliqua.",
    },
    "multiple packets long input": {
        input:    "Lorglpat-ipsu dolor sit amglp|at-t, consglpat-ctg|lpat-tur adipiscing glpat-lit, sglpat-|d do glpat-iusmod t|glpat-mpor incididunt ut |laborglpat-=_ glpat-t dolorglpat-=_ magna aliqua.",
        expected: "Lorglpat-[MASKED] dolor sit amglpat-[MASKED], consglpat-[MASKED] adipiscing glpat-[MASKED], sglpat-[MASKED] do glpat-[MASKED] tglpat-[MASKED] incididunt ut laborglpat-[MASKED] glpat-[MASKED] dolorglpat-[MASKED] magna aliqua.",
    },
    "second long input": {
        input:    "Lorglpat- ipsu dolor sit amglpat-t, consglpat-ctglpat-tur adipiscing glpat-lit, sglpat-d do glpat-iusmod tglpat-mpor incididunt ut laborglpat-=_ glpat-t dolorglpat-=_ magna aliqua.",
        expected: "Lorglpat- ipsu dolor sit amglpat-[MASKED], consglpat-[MASKED] adipiscing glpat-[MASKED], sglpat-[MASKED] do glpat-[MASKED] tglpat-[MASKED] incididunt ut laborglpat-[MASKED] glpat-[MASKED] dolorglpat-[MASKED] magna aliqua.",
    },
    "custom prefix with default one at the beginning of the line": {
        prefixes: []string{"token-"},
        input:    "token-imperdiet Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esse",
        expected: "token-[MASKED] Lorem ipsum dolor sit amet, ex ea commodo in voluptate velit esse",
    },
    "custom prefix with default one multiple packets long input": {
        prefixes: []string{"tok-"},
        input:    "Lortok-ipsu dolor sit amt|ok-t, cons-ctg|lpat-tur adipiscing tok-lit, stok-|d do tok-iusmod t|tok-mpor incididunt ut |labortok-=_ tok-t dolortok-=_ magna aliqua. Tglpat-llus orci ac auctor auguglpat-eee mauris auguglpat-wEr_ lorem",
        expected: "Lortok-[MASKED] dolor sit amtok-[MASKED], cons-ctglpat-[MASKED] adipiscing tok-[MASKED], stok-[MASKED] do tok-[MASKED] ttok-[MASKED] incididunt ut labortok-[MASKED] tok-[MASKED] dolortok-[MASKED] magna aliqua. Tglpat-[MASKED] orci ac auctor auguglpat-[MASKED] mauris auguglpat-[MASKED] lorem",
    },
    "ignored eleventh prefix and more": {
        prefixes: []string{"mask1-", "mask2-", "mask3-", "mask4-", "mask5-", "mask6-", "mask7-", "mask8-", "mask9-", "mask10-", "mask11-"},
        input:    "Lormask1-ipsu dolor sit amm|ask2-t, cons-ctg|lpat-tur adipiscing mask5-lit, smask11-|d do mask7-iusmod t|glpat-mpor incididunt ut |labormask10-=_ mask9-t",
        expected: "Lormask1-[MASKED] dolor sit ammask2-[MASKED], cons-ctglpat-[MASKED] adipiscing mask5-[MASKED], smask11-d do mask7-[MASKED] tglpat-[MASKED] incididunt ut labormask10-=_ mask9-[MASKED]",
    },
    "whitespaced prefixes": {
        prefixes: []string{" mask1- ", "	mask2-", "mask3-	", "mask4-", "mask5-", "mask6-", "mask7-", "mask8-", "mask9-"},
        input:    "Lormask1-ipsu dolor sit amm|ask2-t, cons-ctg|lpat-tur adipiscing mask5-lit, smask11-|d do mask7-iusmod t|glpat-mpor incididunt ut |labormask10-=_ mask9-t",
        expected: "Lormask1-[MASKED] dolor sit ammask2-[MASKED], cons-ctglpat-[MASKED] adipiscing mask5-[MASKED], smask11-d do mask7-[MASKED] tglpat-[MASKED] incididunt ut labormask10-=_ mask9-[MASKED]",
    },
    ];

    let mock = GitlabRunnerMock::start().await;

    for t in test_cases {
        println!("{}", t.name);
        let mut log_job = mock.job_builder(t.name.to_string());
        log_job.add_step(
            MockJobStepName::Script,
            vec!["dummy".to_string()],
            3600,
            MockJobStepWhen::OnSuccess,
            false,
        );

        log_job.add_token_prefix("glpat-".to_string());
        for pfx in t.prefixes.iter().take(9) {
            log_job.add_token_prefix(pfx.to_string());
        }

        log_job.add_artifact_paths(vec!["*".to_string()]);
        let log_job = log_job.build();
        mock.enqueue_job(log_job.clone());

        let dir = tempfile::tempdir().unwrap();

        let (mut runner, layer) = Runner::new_with_layer(
            mock.uri(),
            mock.runner_token().to_string(),
            dir.path().to_path_buf(),
        );

        let subscriber = Registry::default().with(layer);
        async {
            // Upload job comes first
            let inp = t.input.clone();
            let got_job = runner
                .request_job(|_job| async move { Ok(MaskTest { log_text: inp }) })
                .await
                .unwrap();

            assert!(got_job);
            runner.wait_for_space(1).await;
            assert_eq!(MockJobState::Success, log_job.state());

            let data = log_job.log();
            println!(
                "IN: {}\n LEFT: {} RIGHT: {}",
                t.input,
                String::from_utf8_lossy(data.as_slice()),
                t.expected
            );
            assert_eq!(data.as_slice(), t.expected.as_bytes());
        }
        .with_subscriber(subscriber)
        .await;

        let data = {
            let raw = mock
                .get_job_artifact(log_job.id())
                .expect("failed to get job artifact");
            let mut z =
                ZipArchive::new(Cursor::new(raw)).expect("failed to open job artifact as zip");
            let mut f = z
                .by_name("masked")
                .expect("failed to obtain masked log file");
            let mut data = Vec::new();
            f.read_to_end(&mut data)
                .expect("failed to read data from masked log file");
            data
        };

        println!(
            "IN: {}\n LEFT: {} RIGHT: {}",
            t.input,
            String::from_utf8_lossy(data.as_slice()),
            t.expected
        );
        assert_eq!(data.as_slice(), t.expected.as_bytes());
    }
}
