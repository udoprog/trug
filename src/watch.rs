//! Watches source files and rebuilds, one build at a time.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::report::{Event, Reporter};

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

/// Content hashes of watched files, so a write that leaves a file as it was
/// (a hook regenerating identical output) is not a change.
#[derive(Debug, Default)]
struct Contents(HashMap<PathBuf, Option<u64>>);

impl Contents {
    fn scan(&mut self, dir: &Path, ignore: &Ignore) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if ignore.is_ignored(&path) {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => self.scan(&path, ignore),
                Ok(t) if t.is_file() => {
                    self.0.insert(path.clone(), hash_file(&path));
                }
                _ => {}
            }
        }
    }

    /// Records the path's current content; true when it differs from the last
    /// recorded one. Directories and unreadable paths count as changed.
    fn changed(&mut self, path: &Path) -> bool {
        if path.is_dir() {
            return true;
        }
        let hash = hash_file(path);
        self.0.insert(path.to_owned(), hash) != Some(hash)
    }
}

fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Runs `build` after changes: waits for `debounce` of quiet, builds, and
/// when changes arrived during the build builds once more. `on_built` fires
/// after each build that returns `Ok`; a failed build goes to `reporter`. Returns when `changes`
/// is closed.
pub fn rebuild_loop<E: fmt::Display>(
    changes: Receiver<()>,
    debounce: Duration,
    reporter: &Reporter,
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
            Err(error) => reporter.emit(&Event::BuildFailed(&error.to_string())),
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
    /// Gets watch errors and failed builds; silent by default.
    pub reporter: Reporter,
}

impl Watcher {
    pub fn start<E: fmt::Display>(
        request: WatchRequest,
        build: impl FnMut() -> Result<(), E> + Send + 'static,
        on_built: impl FnMut() + Send + 'static,
    ) -> Result<Self, Error> {
        let (tx, rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let ignore = request.ignore;
        let reporter = request.reporter;
        let watch_reporter = reporter.clone();
        let mut contents = Contents::default();
        for path in &request.paths {
            contents.scan(path, &ignore);
        }
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    // Every path is checked, so each content is recorded.
                    let changed = !event.kind.is_access()
                        && event
                            .paths
                            .iter()
                            .filter(|path| !ignore.is_ignored(path))
                            .map(|path| contents.changed(path))
                            .collect::<Vec<_>>()
                            .contains(&true);
                    if changed {
                        let _ = tx.send(());
                    }
                }
                Err(error) => {
                    watch_reporter.emit(&Event::Warning(&format!("watch error: {error}")))
                }
            })?;
        for path in &request.paths {
            watcher.watch(path, RecursiveMode::Recursive)?;
        }
        let debounce = request.debounce;
        let thread =
            std::thread::spawn(move || rebuild_loop(rx, debounce, &reporter, build, on_built));
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
                    &Reporter::default(),
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
        let errors = Arc::new(Mutex::new(Vec::new()));
        let failed = errors.clone();
        rebuild_loop(
            rx,
            Duration::from_millis(1),
            &Reporter::new(move |event| {
                if let Event::BuildFailed(error) = event {
                    failed.lock().unwrap().push(error.to_string());
                }
            }),
            || Err::<(), _>("boom"),
            || swaps += 1,
        );
        assert_eq!(swaps, 0);
        assert_eq!(*errors.lock().unwrap(), ["boom"]);
    }

    #[test]
    fn identical_rewrites_settle_and_changed_content_rebuilds() {
        let dir = std::env::temp_dir().join(format!("trug-watch-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("gen.scss"), "same").unwrap();
        let generated = dir.join("gen.scss");
        let counter = Arc::new(Mutex::new(0u32));
        let content = Arc::new(Mutex::new(String::from("same")));
        let (tx, rx) = mpsc::channel();
        let watcher = Watcher::start(
            WatchRequest {
                paths: vec![dir.clone()],
                ignore: Ignore::new(&dir, &[], vec![], None).unwrap(),
                debounce: Duration::from_millis(50),
                reporter: Reporter::default(),
            },
            {
                let counter = counter.clone();
                let content = content.clone();
                move || {
                    *counter.lock().unwrap() += 1;
                    std::fs::write(&generated, &*content.lock().unwrap()).unwrap();
                    Ok::<_, String>(())
                }
            },
            move || {
                let _ = tx.send(());
            },
        )
        .unwrap();
        std::fs::write(dir.join("a.scss"), "x").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        assert_eq!(*counter.lock().unwrap(), 1);

        *content.lock().unwrap() = "changed".into();
        std::fs::write(dir.join("a.scss"), "y").unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        assert_eq!(*counter.lock().unwrap(), 3);
        drop(watcher);
        let _ = std::fs::remove_dir_all(&dir);
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
                reporter: Reporter::default(),
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
