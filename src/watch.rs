//! Watches source files and rebuilds, one build at a time.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub enum Error {
    Pattern(String, globset::Error),
    Notify(notify::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Pattern(pattern, error) => write!(f, "invalid ignore `{pattern}`: {error}"),
            Error::Notify(error) => write!(f, "cannot watch: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<notify::Error> for Error {
    fn from(error: notify::Error) -> Self {
        Error::Notify(error)
    }
}

/// Decides which changed paths are ignored: a path is ignored when it, or any
/// directory above it, matches a pattern (relative to the root) or lies under
/// one of the excluded directories.
#[derive(Debug)]
pub struct Ignore {
    root: PathBuf,
    globs: GlobSet,
    dirs: Vec<PathBuf>,
    staging_prefix: Option<String>,
}

impl Ignore {
    /// `dist` names the output directory whose staging siblings are ignored.
    pub fn new(
        root: &Path,
        patterns: &[String],
        dirs: Vec<PathBuf>,
        dist: Option<&Path>,
    ) -> Result<Self, Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| Error::Pattern(pattern.clone(), e))?;
            builder.add(glob);
        }
        Ok(Self {
            root: root.to_owned(),
            globs: builder
                .build()
                .map_err(|e| Error::Pattern(patterns.join(", "), e))?,
            dirs,
            staging_prefix: dist.map(crate::dist::sibling_prefix),
        })
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        if self.dirs.iter().any(|dir| path.starts_with(dir)) {
            return true;
        }
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        relative.ancestors().any(|p| {
            !p.as_os_str().is_empty() && (self.globs.is_match(p) || self.matches_staging(p))
        })
    }

    fn matches_staging(&self, path: &Path) -> bool {
        let Some(prefix) = &self.staging_prefix else {
            return false;
        };
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(prefix.as_str()))
    }
}

/// Runs `build` after changes: waits for `debounce` of quiet, builds, and
/// when changes arrived during the build builds once more. `on_built` fires
/// after each build that returns `Ok`. Returns when `changes` is closed.
pub fn rebuild_loop<E: fmt::Display>(
    changes: Receiver<()>,
    debounce: Duration,
    mut build: impl FnMut() -> Result<(), E>,
    mut on_built: impl FnMut(),
) {
    while changes.recv().is_ok() {
        loop {
            match changes.recv_timeout(debounce) {
                Ok(()) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        match build() {
            Ok(()) => on_built(),
            Err(error) => eprintln!("trug: build failed: {error}"),
        }
    }
}

/// Watches paths and runs a rebuild loop on its own thread; dropping it stops both.
pub struct Watcher {
    watcher: Option<RecommendedWatcher>,
    thread: Option<JoinHandle<()>>,
}

pub struct WatchRequest {
    pub paths: Vec<PathBuf>,
    pub ignore: Ignore,
    pub debounce: Duration,
}

impl Watcher {
    pub fn start<E: fmt::Display>(
        request: WatchRequest,
        build: impl FnMut() -> Result<(), E> + Send + 'static,
        on_built: impl FnMut() + Send + 'static,
    ) -> Result<Self, Error> {
        let (tx, rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let ignore = request.ignore;
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    if !event.kind.is_access()
                        && event.paths.iter().any(|path| !ignore.is_ignored(path))
                    {
                        let _ = tx.send(());
                    }
                }
                Err(error) => eprintln!("trug: watch error: {error}"),
            })?;
        for path in &request.paths {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
        let debounce = request.debounce;
        let thread = std::thread::spawn(move || rebuild_loop(rx, debounce, build, on_built));
        Ok(Self {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn ignore(patterns: &[&str]) -> Ignore {
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        Ignore::new(
            Path::new("/app"),
            &patterns,
            vec![PathBuf::from("/app/dist"), PathBuf::from("/ws/target")],
            Some(Path::new("/app/out")),
        )
        .unwrap()
    }

    #[test]
    fn ignores_globs_dirs_and_staging() {
        let ignore = ignore(&["style/generated/**", "*.tmp", "target"]);
        assert!(ignore.is_ignored(Path::new("/app/style/generated/a.scss")));
        assert!(ignore.is_ignored(Path::new("/app/x/y.tmp")));
        assert!(ignore.is_ignored(Path::new("/app/target/debug/foo")));
        assert!(ignore.is_ignored(Path::new("/app/dist/index.html")));
        assert!(ignore.is_ignored(Path::new("/ws/target/debug/x")));
        assert!(ignore.is_ignored(Path::new("/app/.out.trug-staging-1/a.js")));
        assert!(!ignore.is_ignored(Path::new("/app/.dist.trug-staging-1/a.js")));
        assert!(!ignore.is_ignored(Path::new("/app/style/main.scss")));
        assert!(!ignore.is_ignored(Path::new("/app/src/lib.rs")));
    }

    #[test]
    fn many_changes_make_one_build_and_changes_during_a_build_queue_one() {
        let (tx, rx) = mpsc::channel();
        let builds = Arc::new(Mutex::new(0u32));
        let swaps = Arc::new(Mutex::new(0u32));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let thread = {
            let builds = builds.clone();
            let swaps = swaps.clone();
            std::thread::spawn(move || {
                rebuild_loop(
                    rx,
                    Duration::from_millis(30),
                    move || {
                        *builds.lock().unwrap() += 1;
                        started_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                        Ok::<_, String>(())
                    },
                    move || *swaps.lock().unwrap() += 1,
                )
            })
        };
        for _ in 0..20 {
            tx.send(()).unwrap();
        }
        started_rx.recv().unwrap();
        for _ in 0..20 {
            tx.send(()).unwrap();
        }
        go_tx.send(()).unwrap();
        started_rx.recv().unwrap();
        go_tx.send(()).unwrap();
        drop(tx);
        thread.join().unwrap();
        assert_eq!(*builds.lock().unwrap(), 2);
        assert_eq!(*swaps.lock().unwrap(), 2);
    }

    #[test]
    fn failed_build_does_not_fire_the_hook() {
        let (tx, rx) = mpsc::channel();
        tx.send(()).unwrap();
        drop(tx);
        let mut swaps = 0;
        rebuild_loop(
            rx,
            Duration::from_millis(1),
            || Err::<(), _>("boom"),
            || swaps += 1,
        );
        assert_eq!(swaps, 0);
    }

    #[test]
    fn watcher_rebuilds_on_file_change() {
        let dir = std::env::temp_dir().join(format!("trug-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = Watcher::start(
            WatchRequest {
                paths: vec![dir.clone()],
                ignore: Ignore::new(&dir, &["*.skip".into()], vec![], None).unwrap(),
                debounce: Duration::from_millis(50),
            },
            || Ok::<_, String>(()),
            move || {
                let _ = tx.send(());
            },
        )
        .unwrap();
        std::fs::write(dir.join("a.skip"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
        std::fs::write(dir.join("a.scss"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
