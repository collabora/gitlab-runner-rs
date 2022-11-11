//! Helpers to upload to gitlab
use std::fs::File;
use std::future::Future;
use std::io::{Seek, Write};
use std::path::Path;
use std::pin::Pin;
use std::task::Poll;
use std::thread;

use flate2::{Compression, GzBuilder};
use futures::{future::BoxFuture, AsyncWrite, FutureExt};
use reqwest::Body;
use tokio::fs::File as AsyncFile;
use tokio::sync::mpsc::{self, error::SendError};
use tokio::sync::oneshot;
use tokio_util::io::ReaderStream;
use tracing::{error, warn};

use crate::{
    client::{ArtifactFormat, Client, JobArtifact},
    JobResult,
};

#[derive(Debug)]
enum UploadRequest {
    NewFile(String, oneshot::Sender<std::io::Result<()>>),
    WriteData(Vec<u8>, oneshot::Sender<std::io::Result<()>>),
    Finish(oneshot::Sender<std::io::Result<File>>),
}

enum UploadFileState<'a> {
    Idle,
    Writing(
        Option<BoxFuture<'a, Result<(), SendError<UploadRequest>>>>,
        oneshot::Receiver<std::io::Result<()>>,
    ),
}

fn zip_thread(mut temp: File, mut rx: mpsc::Receiver<UploadRequest>) {
    let mut zip = zip::ZipWriter::new(&mut temp);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);

    loop {
        if let Some(request) = rx.blocking_recv() {
            match request {
                UploadRequest::NewFile(s, tx) => {
                    let r = zip.start_file(s, options);
                    tx.send(r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)))
                        .expect("Couldn't send reply");
                }
                UploadRequest::WriteData(v, tx) => {
                    let r = zip.write(&v);
                    tx.send(r.and(Ok(()))).expect("Couldn't send reply");
                }
                UploadRequest::Finish(tx) => {
                    let r = zip.finish();
                    drop(zip);
                    let reply = match r {
                        Ok(_) => temp.rewind().map(|()| temp),
                        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                    };
                    tx.send(reply).expect("Couldn't send finished zip");
                    return;
                }
            }
        } else {
            return;
        }
    }
}

fn gzip_thread(mut temp: File, mut rx: mpsc::Receiver<UploadRequest>) {
    let mut gz = match rx.blocking_recv() {
        Some(UploadRequest::NewFile(s, tx)) => {
            tx.send(Ok(())).expect("Couldn't send reply");
            GzBuilder::new()
                .filename(s)
                .write(&mut temp, Compression::default())
        }
        Some(UploadRequest::WriteData(_, tx)) => {
            tx.send(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no file open",
            )))
            .expect("Couldn't send reply");
            return;
        }
        Some(UploadRequest::Finish(tx)) => {
            tx.send(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "no file open",
            )))
            .expect("Couldn't send reply");
            return;
        }
        None => {
            return;
        }
    };

    loop {
        if let Some(request) = rx.blocking_recv() {
            match request {
                UploadRequest::NewFile(_, tx) => {
                    tx.send(Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "multiple files not permitted in gzip",
                    )))
                    .expect("Couldn't send reply");
                    return;
                }
                UploadRequest::WriteData(v, tx) => {
                    let r = gz.write(&v);
                    tx.send(r.and(Ok(()))).expect("Couldn't send reply");
                }
                UploadRequest::Finish(tx) => {
                    let r = gz.finish();
                    let reply = match r {
                        Ok(_) => temp.rewind().map(|()| temp),
                        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                    };
                    tx.send(reply).expect("Couldn't send finished gzip");
                    return;
                }
            }
        } else {
            return;
        }
    }
}

/// Single file to be uploaded
pub struct UploadFile<'a> {
    tx: &'a mpsc::Sender<UploadRequest>,
    state: UploadFileState<'a>,
}

impl<'a> AsyncWrite for UploadFile<'a> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            match this.state {
                UploadFileState::Idle => {
                    let (tx, rx) = oneshot::channel();
                    let send = this
                        .tx
                        .send(UploadRequest::WriteData(Vec::from(buf), tx))
                        .boxed();
                    this.state = UploadFileState::Writing(Some(send), rx)
                }
                UploadFileState::Writing(ref mut send, ref mut rx) => {
                    if let Some(f) = send {
                        // TODO error handling
                        let _r = futures::ready!(f.as_mut().poll(cx));
                        *send = None;
                    } else {
                        let _r = futures::ready!(Pin::new(rx).poll(cx));
                        this.state = UploadFileState::Idle;
                        return Poll::Ready(Ok(buf.len()));
                    }
                }
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// An upload to gitlab
pub struct Uploader {
    client: Client,
    job_id: u64,
    job_token: String,
    tx: mpsc::Sender<UploadRequest>,
}

impl Uploader {
    pub(crate) fn new(
        client: Client,
        build_dir: &Path,
        job_id: u64,
        job_token: String,
        artifact: &JobArtifact,
    ) -> Result<Self, ()> {
        let temp = tempfile::tempfile_in(build_dir)
            .map_err(|e| warn!("Failed to create artifacts temp file: {:?}", e))?;

        let (tx, rx) = mpsc::channel(2);
        match artifact.artifact_format {
            ArtifactFormat::Zip => {
                thread::spawn(move || zip_thread(temp, rx));
            }
            ArtifactFormat::Gzip => {
                thread::spawn(move || gzip_thread(temp, rx));
            }
            ArtifactFormat::Raw => {
                error!("Raw artifacts are currently unsupported.");
                return Err(());
            }
        }
        Ok(Self {
            client,
            job_id,
            job_token,
            tx,
        })
    }

    /// Create a new file to be uploaded
    pub(crate) async fn file(&mut self, name: String) -> Result<UploadFile<'_>, ()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(UploadRequest::NewFile(name, tx))
            .await
            .expect("Failed to create file");
        match rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                warn!("Failed to create compressed artifact file: {:?}", err);
                Err(())
            }
            Err(_) => {
                warn!("Failed to compress artifacts: thread died");
                Err(())
            }
        }?;

        Ok(UploadFile {
            tx: &self.tx,
            state: UploadFileState::Idle,
        })
    }

    pub(crate) async fn upload(self) -> JobResult {
        let (tx, rx) = oneshot::channel();
        self.tx.send(UploadRequest::Finish(tx)).await.unwrap();
        let file = AsyncFile::from_std(match rx.await {
            Ok(Ok(file)) => Ok(file),
            Ok(Err(err)) => {
                warn!("Failed to zip artifacts: {:?}", err);
                Err(())
            }
            Err(_) => {
                warn!("Failed to zip artifacts: thread died");
                Err(())
            }
        }?);

        let reader = ReaderStream::new(file);
        self.client
            .upload_artifact(
                self.job_id,
                &self.job_token,
                "artifacts.zip",
                Body::wrap_stream(reader),
            )
            .await
            .map_err(|e| {
                warn!("Failed to upload artifacts: {:?}", e);
            })
    }
}
