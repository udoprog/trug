//! What the library says while it works: events sent to a [`Reporter`], which is silent unless
//! the caller gives it a callback.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Something the build is doing or has done.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    /// A build step started. `child_output` when a child process (cargo, a hook) writes to the
    /// terminal while it runs.
    StepStarted {
        step: &'static str,
        child_output: bool,
    },
    /// The step that started last finished.
    StepFinished { step: &'static str, took: Duration },
    /// The step that started last failed; the error follows as the build's result.
    StepFailed { step: &'static str, took: Duration },
    /// A command is about to run, as a shell would show it.
    Command(&'a str),
    /// A tool was found and will be used from `path`.
    Tool { name: &'a str, path: &'a Path },
    /// A tool download started.
    Downloading { name: &'a str, version: &'a str },
    /// Bytes of the download of `name` received so far; `total` when the server says.
    DownloadProgress {
        name: &'a str,
        done: u64,
        total: Option<u64>,
    },
    /// A tool download finished.
    Downloaded { name: &'a str, version: &'a str },
    /// A tool download failed; the error follows as a warning or the result.
    DownloadFailed { name: &'a str },
    /// A tool is being built with `cargo install`, whose output goes to the terminal.
    Installing { name: &'a str, version: &'a str },
    /// A warning: Sass `@warn` and deprecations, a missing tool, a watch error.
    Warning(&'a str),
    /// A Sass `@debug`.
    Debug(&'a str),
    /// A rebuild under watch failed with this error.
    BuildFailed(&'a str),
    /// Files changed under watch, relative to the watch root when below it; a rebuild follows.
    Changed(&'a [PathBuf]),
    /// A proxied request failed and got a 502.
    ProxyError(&'a str),
}

type Callback = dyn Fn(&Event<'_>) + Send + Sync;

/// Where events go; [`Reporter::default`] drops them.
#[derive(Clone, Default)]
pub struct Reporter(Option<Arc<Callback>>);

impl Reporter {
    pub fn new(callback: impl Fn(&Event<'_>) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(callback)))
    }

    pub fn emit(&self, event: &Event<'_>) {
        if let Some(callback) = &self.0 {
            callback(event);
        }
    }
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() {
            "Reporter(..)"
        } else {
            "Reporter(silent)"
        })
    }
}

/// `command` as a shell line, for events and errors.
pub(crate) fn command_line(command: &std::process::Command) -> String {
    let mut line = quote(&command.get_program().to_string_lossy());
    for arg in command.get_args() {
        line.push(' ');
        line.push_str(&quote(&arg.to_string_lossy()));
    }
    line
}

fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_quote_what_a_shell_would_split() {
        let mut command = std::process::Command::new("cargo");
        command.args(["build", "--features", "a b", "it's"]);
        assert_eq!(
            command_line(&command),
            r"cargo build --features 'a b' 'it'\''s'"
        );
    }
}
