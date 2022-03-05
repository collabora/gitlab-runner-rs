use std::{
    io::{Read, Seek},
    path::Path,
};

use zip::read::ZipFile;

pub struct ArtifactFile<'a>(ZipFile<'a>);

impl ArtifactFile<'_> {
    pub fn name(&self) -> &str {
        self.0.name()
    }

    pub fn path(&self) -> Option<&Path> {
        self.0.enclosed_name()
    }
}

impl Read for ArtifactFile<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

pub(crate) trait ReadSeek: Read + Seek {}
impl<T> ReadSeek for T where T: Read + Seek {}

impl<T> From<T> for Artifact
where
    T: Send + Read + Seek + 'static,
{
    fn from(data: T) -> Self {
        Artifact::new(Box::new(data))
    }
}

pub struct Artifact {
    zip: zip::ZipArchive<Box<dyn ReadSeek + Send>>,
}

impl Artifact {
    pub(crate) fn new(data: Box<dyn ReadSeek + Send>) -> Self {
        //let reader = std::io::Cursor::new(data);
        let zip = zip::ZipArchive::new(data).unwrap();
        Self { zip }
    }

    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.zip.file_names()
    }

    pub fn file(&mut self, name: &str) -> Option<ArtifactFile> {
        self.zip.by_name(name).ok().map(ArtifactFile)
    }

    pub fn by_index(&mut self, i: usize) -> Option<ArtifactFile> {
        self.zip.by_index(i).ok().map(ArtifactFile)
    }

    pub fn is_empty(&self) -> bool {
        self.zip.is_empty()
    }

    pub fn len(&self) -> usize {
        self.zip.len()
    }
}
