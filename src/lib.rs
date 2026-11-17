//! Builds an app's CSS from its own Sass and the Sass its crates ship.
//!
//! A crate opts in with `[package.metadata.trug]` (`dir`, default `"style"`;
//! `entry`, default `"_index.scss"`, inside `dir`). The app's stylesheets then
//! `@use "pkg:<crate>"`, `@use "pkg:<crate>/<path>"`, and read which of the
//! crate's features this build enables from `@use "pkg:<crate>/features"`.

pub mod importer;
pub mod packages;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use importer::PackageImporter;
pub use packages::{Query, StylePackage};
pub use sasso::OutputStyle;

#[derive(Debug)]
pub enum Error {
    Packages(packages::Error),
    Sass(sasso::Error),
    Io(PathBuf, io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Packages(error) => error.fmt(f),
            Error::Sass(error) => error.fmt(f),
            Error::Io(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

impl From<packages::Error> for Error {
    fn from(error: packages::Error) -> Self {
        Error::Packages(error)
    }
}

/// Compiles the stylesheet at `entry` with `packages` available as `pkg:` URLs.
pub fn compile(
    entry: &Path,
    packages: Vec<StylePackage>,
    load_paths: Vec<PathBuf>,
    style: OutputStyle,
) -> Result<String, Error> {
    let entry = std::path::absolute(entry).map_err(|e| Error::Io(entry.to_owned(), e))?;
    let source = std::fs::read_to_string(&entry).map_err(|e| Error::Io(entry.clone(), e))?;
    let importer = PackageImporter::new(packages, load_paths);
    let url = entry.to_string_lossy();
    let options = sasso::Options::new()
        .with_importer(&importer)
        .with_url(&url)
        .with_style(style);
    sasso::compile(&source, &options).map_err(Error::Sass)
}

/// Writes `contents` to `path` through a temporary file beside it, so a reader
/// never sees a partial file and a failed write leaves the old one.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), Error> {
    let io = |e| Error::Io(path.to_owned(), e);
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".trug-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents).map_err(|e| Error::Io(tmp.clone(), e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}
