use std::io::Read;

use futures::AsyncWriteExt;
use gitlab_runner::job::Job;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{outputln, JobHandler, JobResult, Phase, Runner};
use serde::Deserialize;
use structopt::StructOpt;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info};
use tracing_subscriber::prelude::*;
use url::Url;

#[derive(StructOpt)]
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

#[async_trait::async_trait]
impl JobHandler for Run {
    async fn step(&mut self, script: &[String], _phase: Phase) -> JobResult {
        for command in script {
            self.command(command).await?;
        }

        Ok(())
    }

    async fn upload_artifacts(&mut self, upload: &mut Uploader) -> JobResult {
        let mut file = upload.file("README.md".to_string()).await;
        file.write_all(b"Demo runner artifact\n")
            .await
            .expect("Failed to write README");
        drop(file);
        for (i, f) in self.facts.iter().enumerate() {
            let mut file = upload.file(format!("fact_{}.txt", i)).await;
            file.write_all(f.as_bytes())
                .await
                .expect("Failed to write fact");
        }
        Ok(())
    }

    async fn cleanup(&mut self) {
        info!("Finished job");
    }
}

#[tokio::main]
async fn main() {
    let opts = Opts::from_args();
    let dir = tempfile::tempdir().unwrap();

    let (mut runner, layer) =
        Runner::new_with_layer(opts.server, opts.token, dir.path().to_path_buf());

    tracing_subscriber::Registry::default()
        .with(
            tracing_subscriber::fmt::Layer::new()
                .pretty()
                .with_filter(tracing::metadata::LevelFilter::INFO),
        )
        .with(layer)
        .init();

    info!("Using {} as build storage prefix", dir.path().display());

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
}
