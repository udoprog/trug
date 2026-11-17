//! `Trug.toml`: what to build and where it goes.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const FILE_NAME: &str = "Trug.toml";

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub serve: Serve,
    #[serde(default)]
    pub watch: Watch,
    #[serde(default)]
    pub hooks: Vec<Hook>,
    #[serde(default)]
    pub proxy: Vec<Proxy>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    /// The index.html to build, relative to `Trug.toml`.
    pub target: Option<PathBuf>,
    /// The output directory, relative to `Trug.toml`; `dist` beside the target by default.
    pub dist: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Serve {
    /// Addresses to listen on; `127.0.0.1` by default.
    pub addresses: Option<Vec<String>>,
    pub port: Option<u16>,
    /// Headers added to every response.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    /// Paths to watch, relative to `Trug.toml`; its directory by default.
    pub watch: Option<Vec<PathBuf>>,
    /// Glob patterns, relative to `Trug.toml`, of paths whose changes are ignored.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Quiet time before a rebuild starts, in milliseconds; 200 by default.
    pub debounce_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proxy {
    /// Backend URL; its path is the mount prefix unless `rewrite` is set.
    pub backend: String,
    /// Path prefix served by trug that is replaced by the backend's path.
    pub rewrite: Option<String>,
    /// Hand backend redirects to the client instead of following them.
    #[serde(default)]
    pub no_redirect: bool,
    /// Proxy WebSocket connections; `backend` is then a `ws://` URL.
    #[serde(default)]
    pub ws: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    PreBuild,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub stage: Stage,
    pub command: String,
    #[serde(default)]
    pub command_arguments: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, error) => write!(f, "{}: {error}", path.display()),
            Error::Parse(path, error) => write!(f, "{}: {error}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

impl Config {
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_owned(), e))?;
        Self::parse(&text).map_err(|e| Error::Parse(path.to_owned(), e))
    }
}

/// The paths a build works with, all absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// Hooks run here.
    pub root: PathBuf,
    pub target: PathBuf,
    pub dist: PathBuf,
}

impl Config {
    pub fn paths(&self, root: &Path) -> Paths {
        let target = root.join(
            self.build
                .target
                .as_deref()
                .unwrap_or(Path::new("index.html")),
        );
        let dist = match &self.build.dist {
            Some(dist) => root.join(dist),
            None => target.parent().unwrap_or(root).join("dist"),
        };
        Paths {
            root: root.to_owned(),
            target,
            dist,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_build_and_hooks() {
        let config = Config::parse(
            r#"
[build]
target = "web/index.html"
dist = "out"

[[hooks]]
stage = "pre_build"
command = "cargo"
command_arguments = ["run", "-p", "tools"]

[[hooks]]
stage = "pre_build"
command = "true"
"#,
        )
        .unwrap();
        assert_eq!(
            config.build.target.as_deref(),
            Some(Path::new("web/index.html"))
        );
        assert_eq!(config.hooks.len(), 2);
        assert_eq!(config.hooks[0].command_arguments, ["run", "-p", "tools"]);
        assert!(config.hooks[1].command_arguments.is_empty());
        let paths = config.paths(Path::new("/app"));
        assert_eq!(paths.target, Path::new("/app/web/index.html"));
        assert_eq!(paths.dist, Path::new("/app/out"));
    }

    #[test]
    fn defaults_dist_beside_target() {
        let paths = Config::default().paths(Path::new("/app"));
        assert_eq!(paths.target, Path::new("/app/index.html"));
        assert_eq!(paths.dist, Path::new("/app/dist"));
        let config = Config::parse("[build]\ntarget = \"w/index.html\"").unwrap();
        assert_eq!(
            config.paths(Path::new("/app")).dist,
            Path::new("/app/w/dist")
        );
    }

    #[test]
    fn rejects_unknown_keys_and_stages() {
        assert!(Config::parse("[serve]\nspeed = 1").is_err());
        assert!(Config::parse("[build]\nminify = true").is_err());
        assert!(Config::parse("[[hooks]]\nstage = \"post_build\"\ncommand = \"x\"").is_err());
    }
}
