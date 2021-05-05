use std::{io::Read, path::Path};

use bytes::Bytes;
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

pub struct Artifact {
    zip: zip::ZipArchive<std::io::Cursor<Bytes>>,
}

impl Artifact {
    pub(crate) fn new(data: Bytes) -> Self {
        let reader = std::io::Cursor::new(data);
        let zip = zip::ZipArchive::new(reader).unwrap();
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

    pub fn len(&self) -> usize {
        self.zip.len()
    }
}
