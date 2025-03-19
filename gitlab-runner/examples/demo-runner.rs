use std::borrow::Cow;
use std::io::{IsTerminal, Read};

use anyhow::{Context, Result};
use clap::Parser;
use futures::io::Cursor;
use futures::AsyncRead;
use gitlab_runner::job::Job;
use gitlab_runner::{
    clone_git_repository, outputln, GitlabLayer, JobHandler, JobResult, Phase, RunnerBuilder,
    UploadableFile,
};
use serde::Deserialize;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info};
use tracing_subscriber::prelude::*;
use url::Url;

#[derive(Parser)]
struct Opts {
    server: Url,
    token: String,
}

#[derive(Deserialize)]
struct Fact {
    fact: String,
}

#[derive(Deserialize)]
struct Reply {
    data: Vec<Fact>,
}

#[derive(Debug)]
struct Run {
    client: reqwest::Client,
    amount: u8,
    job: Job,
    facts: Vec<String>,
}

impl Run {
    fn new(job: Job, amount: u8) -> Self {
        let client = reqwest::Client::new();
        Run {
            client,
            amount,
            job,
            facts: Vec::new(),
        }
    }

    async fn get_facts(&self, amount: u8) -> Result<Vec<String>, ()> {
        let mut url = Url::parse("https://catfact.ninja/facts").unwrap();
        url.query_pairs_mut()
            .append_pair("limit", &format!("{}", amount));

        let mut r: Reply = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| ())?
            .json()
            .await
            .map_err(|e| {
                debug!("Fact failure: {:?}", e);
            })?;
        Ok(r.data.drain(..).map(|f| f.fact).collect())
    }

    #[tracing::instrument(skip(self))]
    async fn command(&mut self, command: &str) -> JobResult {
        outputln!("> {}", command);

        let mut p = command.split_whitespace();
        if let Some(cmd) = p.next() {
            debug!("command: >{}<", cmd);
            match cmd {
                "fact" => {
                    let amount = match p.next() {
                        Some(amount) => match amount.parse() {
                            Ok(a) => a,
                            Err(e) => {
                                outputln!("Failed to parse amount: {}\n", e);
                                return Err(());
                            }
                        },
                        _ => self.amount,
                    };

                    let facts = match self.get_facts(amount).await {
                        Ok(f) => f,
                        Err(e) => {
                            outputln!("Failed to get facts :( - {:?}\n", e);
                            Vec::new()
                        }
                    };
                    for f in facts {
                        outputln!("Did you know: {}\n", f);
                        self.facts.push(f);
                    }
                    Ok(())
                }
                "artifacts" => {
                    for d in self.job.dependencies() {
                        outputln!("= Artifacts from : {} =\n", d.name());
                        if let Some(mut artifact) = d.download().await.unwrap() {
                            for i in 0..artifact.len() {
                                let (name, data) = {
                                    let mut f = artifact.by_index(i).expect("Failed to open file");
                                    let mut s = String::new();
                                    if f.read_to_string(&mut s).is_err() {
                                        s.clear();
                                        s.push_str("Failed to read file to text");
                                    }
                                    (f.name().to_string(), s)
                                };
                                outputln!("=== File: {} ===", name);
                                outputln!("{}", data);
                            }
                        }
                    }
                    Ok(())
                }
                "clone" => {
                    let url = match p.next() {
                        Some(url) => url,
                        _ => {
                            outputln!("Missing repo url as first argument");
                            return Err(());
                        }
                    };
                    let path = match p.next() {
                        Some(path) => self.job.build_dir().join(path),
                        _ => {
                            outputln!("Missing repo output path as second argument");
                            return Err(());
                        }
                    };
                    let reference = p.next();

                    let repo_path = clone_git_repository(
                        self.job.build_dir(),
                        url,
                        reference,
                        std::iter::empty::<&str>(),
                        Some(1),
                    )
                    .map_err(|e| outputln!("Failed to checkout repo: {}", e.to_string()))?;

                    std::fs::rename(repo_path, path)
                        .map_err(|e| outputln!("Failed to move repo path: {}", e.to_string()))?;

                    Ok(())
                }
                "checkout" => {
                    let path = match p.next() {
                        Some(path) => self.job.build_dir().join(path),
                        _ => self.job.build_dir().to_path_buf(),
                    };
                    let temp_repo_path = self
                        .job
                        .clone_git_repository()
                        .map_err(|e| outputln!("Failed to checkout repo: {}", e.to_string()))?;

                    outputln!("Checked out to {:?}", temp_repo_path);

                    if !path.exists() {
                        std::fs::create_dir(&path).map_err(|e| {
                            outputln!("Failed to create dir for repo: {}", e.to_string())
                        })?;
                    }

                    let repo_read_dir = temp_repo_path.read_dir().map_err(|e| {
                        outputln!("Failed to enumerate repo dir: {}", e.to_string())
                    })?;
                    for entry in repo_read_dir {
                        let entry = entry.map_err(|e| {
                            outputln!("Failed to read repo file: {}", e.to_string())
                        })?;
                        std::fs::rename(entry.path(), path.join(entry.file_name())).map_err(
                            |e| outputln!("Failed to move repo file: {}", e.to_string()),
                        )?;
                    }

                    std::fs::remove_dir_all(temp_repo_path).map_err(|e| {
                        outputln!("Failed to remove temp directory: {}", e.to_string())
                    })?;

                    outputln!("Moved repo to {:?}", path);

                    Ok(())
                }
                "list-dir" => {
                    let path = match p.next() {
                        Some(path) => self.job.build_dir().join(path),
                        _ => self.job.build_dir().to_path_buf(),
                    };
                    if !path.exists() {
                        outputln!("Path {:?} doesn't exist", path);
                    } else {
                        for entry in path.read_dir().unwrap() {
                            match entry {
                                Ok(entry) => outputln!("{}", entry.file_name().to_str().unwrap()),
                                Err(e) => outputln!("Error: {:?}: {}", e, e.to_string()),
                            }
                        }
                    }
                    Ok(())
                }
                "cat" => {
                    let path = match p.next() {
                        Some(path) => self.job.build_dir().join(path),
                        _ => {
                            outputln!("No path provided");
                            return Err(());
                        }
                    };
                    if !path.exists() {
                        outputln!("Path {:?} doesn't exist", path);
                        Err(())
                    } else {
                        let data = match std::fs::read_to_string(path) {
                            Ok(s) => s,
                            _ => "Failed to read file to text".to_string(),
                        };
                        outputln!("{}", data);
                        Ok(())
                    }
                }
                _ => {
                    outputln!("Unknown command\n");
                    Err(())
                }
            }
        } else {
            outputln!("empty command");
            Err(())
        }
    }
}

#[derive(PartialEq, Eq)]
enum DemoFile {
    ReadMe,
    Fact { index: usize, value: String },
}

#[async_trait::async_trait]
impl UploadableFile for DemoFile {
    type Data<'a> = Box<dyn AsyncRead + Send + Unpin>;

    fn get_path(&self) -> Cow<'_, str> {
        match self {
            Self::ReadMe => "README.md".into(),
            Self::Fact { index, .. } => format!("fact_{}.txt", index).into(),
        }
    }

    async fn get_data(&self) -> Result<Box<dyn AsyncRead + Send + Unpin>, ()> {
        Ok(match self {
            DemoFile::ReadMe => Box::new(b"Demo runner artifact\n".as_slice()),
            DemoFile::Fact { value, .. } => Box::new(Cursor::new(value.clone().into_bytes())),
        })
    }
}

#[async_trait::async_trait]
impl JobHandler<DemoFile> for Run {
    async fn step(&mut self, script: &[String], _phase: Phase) -> JobResult {
        for command in script {
            self.command(command).await?;
        }

        Ok(())
    }

    async fn get_uploadable_files(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = DemoFile> + Send>, ()> {
        let mut res = vec![DemoFile::ReadMe];
        for (i, f) in self.facts.iter().enumerate() {
            res.push(DemoFile::Fact {
                index: i,
                value: f.clone(),
            });
        }
        Ok(Box::new(res.into_iter()))
    }

    async fn cleanup(&mut self) {
        info!("Finished job");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    let dir = tempfile::tempdir().context("Failed to create temporary directory for builds")?;

    let (layer, jobs) = GitlabLayer::new();

    let env = std::env::var("RUNNER_LOG").unwrap_or_else(|_| "demo_runner=info,warn".to_string());

    // Always enable the gitlab_runner::job span specifically as provides the gitlab.job field to
    // correlate gitlab jobs to log entries
    let envfilter = tracing_subscriber::EnvFilter::builder()
        .parse(&env)
        .with_context(|| format!("Failed to parse env filter: {env}"))?
        .add_directive("gitlab_runner::gitlab::job=error".parse().unwrap());

    let fmt = tracing_subscriber::fmt::Layer::new();
    let fmt = if std::io::stdout().is_terminal() {
        fmt.pretty().boxed()
    } else {
        fmt.json().boxed()
    };

    tracing_subscriber::Registry::default()
        .with(fmt.with_filter(envfilter))
        .with(layer)
        .init();

    info!("Using {} as build storage prefix", dir.path().display());

    let mut runner = RunnerBuilder::new(opts.server, opts.token, dir.path(), jobs)
        .version(env!("CARGO_PKG_VERSION"))
        .revision(
            option_env!("VERGEN_GIT_SHA")
                .map(|sha| {
                    if option_env!("VERGEN_GIT_DIRTY") == Some("true") {
                        format!("{}-dirty", sha)
                    } else {
                        sha.to_string()
                    }
                })
                .unwrap_or_else(|| "-".to_string()),
        )
        .architecture("gitlab-runner-rs")
        .platform(env!("CARGO_BIN_NAME"))
        .build()
        .await;

    let mut term = signal(SignalKind::terminate()).expect("Failed to register signal handler");
    let mut int = signal(SignalKind::interrupt()).expect("Failed to register signal handler");

    tokio::select! {
        r = runner
        .run(
            move |job| async move {
                let amount = match job.variable("AMOUNT") {
                    Some(v) => v.value().parse().expect("Amount not a number"),
                    _ => 1u8,
                };

                info!("Created new job run");
                Ok(Run::new(job, amount))
            },
            8,
        ) => match r {
                Ok(_) => info!("Runner exited"),
                Err(e) => panic!("Failed to pick up new jobs: {}", e)
        },
        _ = term.recv() => info!("Got TERM: exiting!"),
        _ = int.recv() => info!("Got INT: exiting!"),
    }
    Ok(())
}
