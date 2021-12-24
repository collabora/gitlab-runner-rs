use std::io::Read;

use futures::AsyncWriteExt;
use gitlab_runner::job::Job;
use gitlab_runner::uploader::Uploader;
use gitlab_runner::{JobHandler, JobResult, Phase, Runner};
use log::{debug, info};
use serde::Deserialize;
use structopt::StructOpt;
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

    async fn command(&mut self, command: &str) -> JobResult {
        self.job.trace(format!("> {}\n", command));
        let mut p = command.split_whitespace();
        if let Some(cmd) = p.next() {
            debug!("command: >{}<", cmd);
            match cmd {
                "fact" => {
                    let amount = match p.next() {
                        Some(amount) => match amount.parse() {
                            Ok(a) => a,
                            Err(e) => {
                                self.job.trace(format!("Failed to parse amount: {}\n", e));
                                return Err(());
                            }
                        },
                        _ => self.amount,
                    };

                    let facts = match self.get_facts(amount).await {
                        Ok(f) => f,
                        Err(e) => {
                            self.job
                                .trace(format!("Failed to get facts :( - {:?}\n", e));
                            Vec::new()
                        }
                    };
                    for f in facts {
                        self.job.trace(format!("Did you know: {}\n", f));
                        self.facts.push(f);
                    }
                    Ok(())
                }
                "artifacts" => {
                    for d in self.job.dependencies() {
                        self.job.trace(format!("Artifacts from : {}\n", d.name()));
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
                                self.job.trace(format!("=== File: {} ===\n", name));
                                self.job.trace(data);
                            }
                        }
                    }
                    Ok(())
                }
                _ => {
                    self.job.trace("Unknown command\n");
                    Err(())
                }
            }
        } else {
            self.job.trace("empty command");
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
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let opts = Opts::from_args();
    let mut runner = Runner::new(opts.server, opts.token);
    runner
        .run(
            move |job| async move {
                let amount = match job.variable("AMOUNT") {
                    Some(v) => v.value().parse().expect("Amount not a number"),
                    _ => 1u8,
                };

                info!("Created run");
                Ok(Run::new(job, amount))
            },
            8,
        )
        .await
        .expect("Couldn't pick up jobs");
}
