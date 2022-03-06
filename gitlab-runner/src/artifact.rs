//! Helpers for artifacts downloaded from gitlab
use std::{
    io::{Read, Seek},
    path::Path,
};

use zip::read::ZipFile;

/// A file in a gitlab artifact
///
/// Most importantly this implements [`Read`] to read out the content of the file
pub struct ArtifactFile<'a>(ZipFile<'a>);

impl ArtifactFile<'_> {
    /// Get the name of the file
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// Get the safe path of the file
    ///
    /// The path will be safe to use, that is to say it contains no NUL bytes and will be a
    /// relative path that doesn't go outside of the root.
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

/// An artifact downloaded from gitlab
///
/// The artifact holds a set of files which can be read out one by one
pub struct Artifact {
    zip: zip::ZipArchive<Box<dyn ReadSeek + Send>>,
}

impl Artifact {
    pub(crate) fn new(data: Box<dyn ReadSeek + Send>) -> Self {
        //let reader = std::io::Cursor::new(data);
        let zip = zip::ZipArchive::new(data).unwrap();
        Self { zip }
    }

    /// Iterator of the files names inside the artifacts
    ///
    /// The returned file name isn't sanatized in any way and should *not* be used as a path on the
    /// filesystem. To get a safe path of a given file use [`ArtifactFile::path`]
    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.zip.file_names()
    }

    /// Get a file in the artifact by name
    pub fn file(&mut self, name: &str) -> Option<ArtifactFile> {
        self.zip.by_name(name).ok().map(ArtifactFile)
    }

    /// Get a file in the artifact by index
    pub fn by_index(&mut self, i: usize) -> Option<ArtifactFile> {
        self.zip.by_index(i).ok().map(ArtifactFile)
    }

    /// Checks whether the artifact has no files
    pub fn is_empty(&self) -> bool {
        self.zip.is_empty()
    }

    /// The number of files in the artifacts
    pub fn len(&self) -> usize {
        self.zip.len()
    }
}
