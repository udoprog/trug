//! The build pipeline: hooks, cargo, wasm-bindgen, the output index.html, the dist swap.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cargo_metadata::diagnostic::DiagnosticLevel;
use cargo_metadata::{Message, Metadata};
use sha2::{Digest, Sha256};

use crate::config::{Config, Hook, Paths, Stage};
use crate::dist::Staging;
use crate::html::{self, Directive};
use crate::packages::Query;
use crate::report::{Event, Reporter, command_line};

pub const WASM_TARGET: &str = "wasm32-unknown-unknown";

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    Packages(crate::packages::Error),
    Html(String),
    Hook(String),
    /// `message` is the one-line failure; `output` holds cargo's error diagnostics and
    /// build-script output, which already went to the terminal.
    Cargo {
        message: String,
        output: String,
    },
    Bindgen(String),
    Css(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, error) => write!(f, "{}: {error}", path.display()),
            Error::Packages(error) => error.fmt(f),
            Error::Html(m)
            | Error::Hook(m)
            | Error::Cargo { message: m, .. }
            | Error::Bindgen(m)
            | Error::Css(m) => m.fmt(f),
        }
    }
}

impl Error {
    /// The error with what the terminal already showed of cargo, without ANSI codes: for a page.
    pub fn detail(&self) -> String {
        match self {
            Error::Cargo { message, output } if !output.is_empty() => {
                format!("{message}\n\n{}", strip_ansi(output).trim_end())
            }
            _ => self.to_string(),
        }
    }
}

/// `text` without ANSI escape sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
        } else if chars.next_if_eq(&'[').is_some() {
            while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
        }
    }
    out
}

impl std::error::Error for Error {}

impl From<crate::packages::Error> for Error {
    fn from(error: crate::packages::Error) -> Self {
        Error::Packages(error)
    }
}

#[derive(Clone, Debug)]
pub struct BuildRequest {
    pub config: Config,
    /// The directory holding Trug.toml.
    pub root: PathBuf,
    /// Cargo flags; `manifest_path` is overridden by the rust link.
    pub query: Query,
    pub release: bool,
    pub jobs: Option<u32>,
    /// Prefix of every URL in the output html; overrides the document's base tag.
    pub public_url: Option<String>,
    /// Passes `--quiet` to cargo.
    pub quiet: bool,
    /// Passes `--color always` to cargo and keeps colour in the diagnostics written to stderr;
    /// `--color never` and plain diagnostics otherwise.
    pub color: bool,
    /// Gets the build's progress; silent by default.
    pub reporter: Reporter,
}

#[derive(Debug, Default)]
pub struct Report {
    pub steps: Vec<(&'static str, Duration)>,
    pub dist: PathBuf,
    pub public_url: String,
    /// What the build wrote to dist, in the order it was written.
    pub files: Vec<Output>,
    /// Whether wasm-opt ran.
    pub wasm_opt: WasmOpt,
    /// The wasm's size before wasm-opt, when it ran.
    pub wasm_unoptimized: Option<u64>,
}

/// A file or directory the build wrote to dist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub kind: OutputKind,
    /// Relative to dist.
    pub path: PathBuf,
    /// In bytes; the sum of its files for a directory.
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    Html,
    Wasm,
    Js,
    Style,
    Icon,
    /// A copied directory with this many files.
    Assets(usize),
}

/// Whether and why wasm-opt ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WasmOpt {
    /// Dev builds never run it.
    #[default]
    DevBuild,
    /// `[build] wasm_opt = false`.
    Disabled,
    /// No wasm-opt could be found or downloaded.
    Missing,
    Ran,
    /// This wasm was optimized by an earlier build.
    Cached,
}

struct Timer<'a> {
    report: &'a mut Report,
    reporter: &'a Reporter,
}

impl Timer<'_> {
    fn step<T>(
        &mut self,
        step: &'static str,
        child_output: bool,
        f: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        self.reporter
            .emit(&Event::StepStarted { step, child_output });
        let start = Instant::now();
        let result = f();
        let took = start.elapsed();
        self.reporter.emit(&match result {
            Ok(_) => Event::StepFinished { step, took },
            Err(_) => Event::StepFailed { step, took },
        });
        self.report.steps.push((step, took));
        result
    }
}

pub fn build(request: &BuildRequest) -> Result<Report, Error> {
    let paths = request.config.paths(&request.root);
    let mut report = Report {
        dist: paths.dist.clone(),
        ..Report::default()
    };
    let reporter = &request.reporter;
    let mut timer = Timer {
        report: &mut report,
        reporter,
    };
    let html =
        std::fs::read_to_string(&paths.target).map_err(|e| Error::Io(paths.target.clone(), e))?;
    let directives = html::directives(&html);
    let base = html::public_url_base(&html);
    let public_url = normalize_url(
        request
            .public_url
            .as_deref()
            .or_else(|| base.as_ref().and_then(|b| b.attr("href")))
            .unwrap_or("/"),
    );
    timer.report.public_url = public_url.clone();
    let plan = Plan::new(&directives, &paths)?;

    let staging = Staging::create(&paths.dist).map_err(|e| Error::Io(paths.dist.clone(), e))?;

    if !request.config.hooks.is_empty() {
        timer.step("hooks", true, || {
            for hook in &request.config.hooks {
                run_hook(hook, &paths, staging.path(), reporter)?;
            }
            Ok(())
        })?;
    }

    let mut query = request.query.clone();
    query.manifest_path = Some(plan.manifest.clone());
    query.target = Some(WASM_TARGET.to_owned());

    let mut files = Vec::new();
    let links = if plan.styles.is_empty() {
        HashMap::new()
    } else {
        timer.step("styles", false, || {
            stage_styles(
                &plan.styles,
                &query,
                staging.path(),
                &public_url,
                !request.release,
                reporter,
                &mut files,
            )
        })?
    };

    let wasm = timer.step("cargo", true, || cargo_build(&query, request))?;
    let metadata = query.metadata()?;
    let mut bindgen = timer.step("wasm-bindgen", false, || {
        run_bindgen(&metadata, &wasm, reporter)
    })?;
    if !request.release {
        timer.report.wasm_opt = WasmOpt::DevBuild;
    } else if request.config.build.wasm_opt == Some(false) {
        timer.report.wasm_opt = WasmOpt::Disabled;
    } else {
        let before = std::fs::metadata(bindgen.dir.join(&bindgen.wasm)).map(|m| m.len());
        let ran = timer.step("wasm-opt", false, || {
            bindgen.optimize(|| crate::tools::wasm_opt(reporter), reporter)
        })?;
        if matches!(ran, WasmOpt::Ran | WasmOpt::Cached) {
            timer.report.wasm_unoptimized = before.ok();
        }
        timer.report.wasm_opt = ran;
    }

    timer.step("output", false, || {
        let assets = bindgen.stage(staging.path(), &mut files)?;
        let mut links = links;
        links.extend(plan.stage_statics(staging.path(), &public_url, &directives, &mut files)?);
        let mut extra = Vec::new();
        if let Some(base) = &base {
            let tag = html::tag(
                "base",
                &base.attrs,
                &["data-trug-public-url"],
                ("href", &public_url),
            );
            extra.push((base.range.clone(), tag));
        }
        let out = render(
            &html,
            &directives,
            &plan,
            &assets,
            &links,
            &public_url,
            extra,
        );
        let index = staging.path().join("index.html");
        files.push(Output {
            kind: OutputKind::Html,
            path: "index.html".into(),
            size: out.len() as u64,
        });
        std::fs::write(&index, out).map_err(|e| Error::Io(index, e))
    })?;

    timer.step("swap", false, || {
        staging.swap().map_err(|e| Error::Io(paths.dist.clone(), e))
    })?;
    report.files = files;
    Ok(report)
}

pub fn normalize_url(url: &str) -> String {
    let mut url = url.to_owned();
    if !url.ends_with('/') {
        url.push('/');
    }
    url
}

enum Static {
    CopyDir { from: PathBuf, to: PathBuf },
    Icon { from: PathBuf },
}

/// What the document's directives ask for.
struct Plan {
    manifest: PathBuf,
    /// The rust link's index in the directives, if the document has one.
    rust: Option<usize>,
    styles: Vec<Style>,
    /// Directive index and what to do for each copy-dir and icon link.
    statics: Vec<(usize, Static)>,
}

/// A `rel="scss"` or `rel="css"` link: the directive's index and the source file.
struct Style {
    at: usize,
    scss: bool,
    path: PathBuf,
}

impl Plan {
    /// Copies the static files into staging; returns the replacement for each icon link.
    fn stage_statics(
        &self,
        staging: &Path,
        public_url: &str,
        directives: &[Directive],
        files: &mut Vec<Output>,
    ) -> Result<HashMap<usize, String>, Error> {
        let mut links = HashMap::new();
        for (at, item) in &self.statics {
            match item {
                Static::CopyDir { from, to } => {
                    let (count, size) = copy_dir(from, &staging.join(to))?;
                    files.push(Output {
                        kind: OutputKind::Assets(count),
                        path: to.clone(),
                        size,
                    });
                }
                Static::Icon { from } => {
                    let bytes = std::fs::read(from).map_err(|e| Error::Io(from.clone(), e))?;
                    let stem = from
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let ext = from
                        .extension()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let name = hashed_name(&stem, &ext, &bytes);
                    let to = staging.join(&name);
                    files.push(Output {
                        kind: OutputKind::Icon,
                        path: name.clone().into(),
                        size: bytes.len() as u64,
                    });
                    std::fs::write(&to, bytes).map_err(|e| Error::Io(to, e))?;
                    let directive = &directives[*at];
                    let tag = html::tag(
                        "link",
                        &directive.attrs,
                        &["data-trug"],
                        ("href", &format!("{public_url}{name}")),
                    );
                    links.insert(*at, tag);
                }
            }
        }
        Ok(links)
    }
}

/// Copies `from` into `to`; returns how many files and bytes it copied.
fn copy_dir(from: &Path, to: &Path) -> Result<(usize, u64), Error> {
    std::fs::create_dir_all(to).map_err(|e| Error::Io(to.to_owned(), e))?;
    let entries = std::fs::read_dir(from).map_err(|e| Error::Io(from.to_owned(), e))?;
    let (mut count, mut size) = (0, 0);
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io(from.to_owned(), e))?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            let (c, s) = copy_dir(&entry.path(), &target)?;
            count += c;
            size += s;
        } else {
            size += std::fs::copy(entry.path(), &target).map_err(|e| Error::Io(entry.path(), e))?;
            count += 1;
        }
    }
    Ok((count, size))
}

fn relative_target(path: &str) -> Result<PathBuf, Error> {
    let path = Path::new(path);
    let ok = path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !ok || path.as_os_str().is_empty() {
        return Err(Error::Html(format!(
            "data-target-path {:?} must be a relative path inside dist",
            path.display()
        )));
    }
    Ok(path.to_owned())
}

impl Plan {
    fn new(directives: &[Directive], paths: &Paths) -> Result<Self, Error> {
        let dir = paths.target.parent().unwrap_or(&paths.root);
        let mut rust = None;
        let mut styles = Vec::new();
        let mut statics = Vec::new();
        for (at, directive) in directives.iter().enumerate() {
            match directive.attr("rel") {
                Some("rust") => {
                    if rust.replace(at).is_some() {
                        return Err(Error::Html("more than one rel=\"rust\" link".into()));
                    }
                }
                Some(rel @ ("scss" | "css")) => {
                    let href = directive
                        .attr("href")
                        .ok_or_else(|| Error::Html(format!("rel=\"{rel}\" link has no href")))?;
                    styles.push(Style {
                        at,
                        scss: rel == "scss",
                        path: dir.join(href),
                    });
                }
                Some("copy-dir") => {
                    let href = directive
                        .attr("href")
                        .ok_or_else(|| Error::Html("rel=\"copy-dir\" link without href".into()))?;
                    let to = match directive.attr("data-target-path") {
                        Some(path) => relative_target(path)?,
                        None => {
                            PathBuf::from(Path::new(href).file_name().ok_or_else(|| {
                                Error::Html(format!("bad copy-dir href {href:?}"))
                            })?)
                        }
                    };
                    statics.push((
                        at,
                        Static::CopyDir {
                            from: dir.join(href),
                            to,
                        },
                    ));
                }
                Some("icon") => {
                    let href = directive
                        .attr("href")
                        .ok_or_else(|| Error::Html("rel=\"icon\" link without href".into()))?;
                    statics.push((
                        at,
                        Static::Icon {
                            from: dir.join(href),
                        },
                    ));
                }
                other => {
                    return Err(Error::Html(format!(
                        "unsupported data-trug link rel={:?}",
                        other.unwrap_or("")
                    )));
                }
            }
        }
        let href = rust
            .and_then(|at| directives[at].attr("href"))
            .unwrap_or("Cargo.toml");
        let mut manifest = dir.join(href);
        if manifest.is_dir() {
            manifest.push("Cargo.toml");
        }
        Ok(Self {
            manifest,
            rust,
            styles,
            statics,
        })
    }
}

/// Compiles or copies each style into the staging dir under a hashed name; returns the link
/// replacing each directive, by directive index.
fn stage_styles(
    styles: &[Style],
    query: &Query,
    staging: &Path,
    public_url: &str,
    source_map: bool,
    reporter: &Reporter,
    files: &mut Vec<Output>,
) -> Result<HashMap<usize, String>, Error> {
    let mut links = HashMap::new();
    for style in styles {
        let contents = if style.scss {
            let mut css = crate::CssRequest::new(&style.path);
            css.query = query.clone();
            css.source_map = source_map;
            css.reporter = reporter.clone();
            crate::build_css(&css)
                .map_err(|e| Error::Css(format!("{}: {e}", style.path.display())))?
        } else {
            std::fs::read_to_string(&style.path).map_err(|e| Error::Io(style.path.clone(), e))?
        };
        let stem = style
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = hashed_name(&stem, "css", contents.as_bytes());
        let to = staging.join(&name);
        files.push(Output {
            kind: OutputKind::Style,
            path: name.clone().into(),
            size: contents.len() as u64,
        });
        std::fs::write(&to, contents).map_err(|e| Error::Io(to, e))?;
        links.insert(
            style.at,
            format!("<link rel=\"stylesheet\" href=\"{public_url}{name}\">"),
        );
    }
    Ok(links)
}

fn run_hook(hook: &Hook, paths: &Paths, staging: &Path, reporter: &Reporter) -> Result<(), Error> {
    match hook.stage {
        Stage::PreBuild => {}
    }
    let mut command = Command::new(&hook.command);
    command.args(&hook.command_arguments);
    let line = command_line(&command);
    reporter.emit(&Event::Command(&line));
    let status = command
        .current_dir(&paths.root)
        .env("TRUG_STAGING_DIR", staging)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| Error::Hook(format!("hook `{line}`: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Hook(format!("hook `{line}` failed: {status}")))
    }
}

fn cargo_build(query: &Query, request: &BuildRequest) -> Result<PathBuf, Error> {
    let manifest = query.manifest_path.as_ref().expect("manifest set by build");
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--message-format=json-diagnostic-rendered-ansi",
            "--color",
            if request.color { "always" } else { "never" },
            "--target",
        ])
        .arg(WASM_TARGET)
        .arg("--manifest-path")
        .arg(manifest);
    if request.release {
        command.arg("--release");
    }
    if let Some(jobs) = request.jobs {
        command.arg("--jobs").arg(jobs.to_string());
    }
    if !query.features.is_empty() {
        command.arg("--features").arg(query.features.join(","));
    }
    for (on, flag) in [
        (query.all_features, "--all-features"),
        (query.no_default_features, "--no-default-features"),
        (query.locked, "--locked"),
        (query.offline, "--offline"),
        (request.quiet, "--quiet"),
    ] {
        if on {
            command.arg(flag);
        }
    }
    if io::stderr().is_terminal() {
        // Cargo draws no progress bar when its stderr is a pipe.
        command.env("CARGO_TERM_PROGRESS_WHEN", "always");
        command.env("CARGO_TERM_PROGRESS_WIDTH", terminal_width().to_string());
    }
    let line = command_line(&command);
    request.reporter.emit(&Event::Command(&line));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| cargo_error(format!("`{line}`: {e}")))?;
    let stdout = io::BufReader::new(child.stdout.take().expect("piped"));
    let stderr = child.stderr.take().expect("piped");
    let stderr = std::thread::spawn(move || tee_stderr(stderr, io::stderr()));
    let ansi = request.color;
    let mut errors = String::new();
    let wanted = std::fs::canonicalize(manifest).unwrap_or_else(|_| manifest.clone());
    let mut wasm = None;
    let mut found_cdylib = false;
    for message in Message::parse_stream(stdout).flatten() {
        let artifact = match message {
            Message::CompilerMessage(m) => {
                if let Some(rendered) = &m.message.rendered {
                    let _ = if ansi {
                        write!(io::stderr(), "{rendered}")
                    } else {
                        write!(io::stderr(), "{}", strip_ansi(rendered))
                    };
                    if matches!(
                        m.message.level,
                        DiagnosticLevel::Error | DiagnosticLevel::Ice
                    ) {
                        errors.push_str(rendered);
                    }
                }
                continue;
            }
            Message::CompilerArtifact(artifact) => artifact,
            _ => continue,
        };
        let same = std::fs::canonicalize(&artifact.manifest_path).is_ok_and(|path| path == wanted);
        let kinds = |wanted: &str| artifact.target.kind.iter().any(|k| k.to_string() == wanted);
        let is_cdylib = kinds("cdylib");
        if same
            && (is_cdylib || (kinds("bin") && !found_cdylib))
            && let Some(file) = artifact
                .filenames
                .iter()
                .find(|f| f.extension() == Some("wasm"))
        {
            found_cdylib = is_cdylib;
            wasm = Some(file.clone().into_std_path_buf());
        }
    }
    let status = child
        .wait()
        .map_err(|e| cargo_error(format!("cargo: {e}")))?;
    let tail = stderr.join().unwrap_or_default();
    if !status.success() {
        errors.push_str(&tail);
        return Err(Error::Cargo {
            message: format!("`{line}` failed: {status}"),
            output: errors,
        });
    }
    wasm.ok_or_else(|| {
        cargo_error(format!(
            "{} builds no wasm cdylib or bin; add crate-type = [\"cdylib\"] to its [lib]",
            manifest.display()
        ))
    })
}

fn cargo_error(message: String) -> Error {
    Error::Cargo {
        message,
        output: String::new(),
    }
}

/// Copies cargo's stderr to `out` as raw bytes, so progress redraws reach a terminal; returns
/// what follows its first `error` line, which is where build scripts' output and the failure
/// summary start. Text a `\r` overwrites, such as a progress bar, is left out.
fn tee_stderr(mut stderr: impl io::Read, mut out: impl Write) -> String {
    let mut errors = String::new();
    let mut seen = false;
    let mut line = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let _ = out.write_all(&buf[..n]);
        let _ = out.flush();
        for &b in &buf[..n] {
            match b {
                b'\r' => line.clear(),
                b'\n' => {
                    let text = String::from_utf8_lossy(&line);
                    seen = seen || strip_ansi(&text).trim_start().starts_with("error");
                    if seen {
                        errors.push_str(&text);
                        errors.push('\n');
                    }
                    line.clear();
                }
                _ => line.push(b),
            }
        }
    }
    errors
}

/// The width of stderr's terminal, 80 when unknown.
#[cfg(unix)]
fn terminal_width() -> usize {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ fills a winsize and nothing else.
    let ok = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0;
    let cols = if ok {
        usize::from(unsafe { size.assume_init() }.ws_col)
    } else {
        0
    };
    if cols == 0 { 80 } else { cols }
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    80
}

/// The files wasm-bindgen produced.
struct Bindgen {
    dir: PathBuf,
    name: String,
    /// The wasm file in `dir` to stage.
    wasm: String,
}

/// Features current wasm-bindgen output uses, which wasm-opt rejects unless enabled.
const WASM_OPT_FEATURES: &[&str] = &[
    "--enable-bulk-memory",
    "--enable-nontrapping-float-to-int",
    "--enable-reference-types",
    "--enable-sign-ext",
    "--enable-mutable-globals",
    "--enable-multivalue",
];

impl Bindgen {
    fn new(dir: PathBuf, name: String) -> Self {
        let wasm = format!("{name}_bg.wasm");
        Bindgen { dir, name, wasm }
    }

    /// Runs `wasm-opt -Oz` from `find` into a file cached beside the bindgen output; stages the
    /// unoptimized wasm when `find` has none.
    fn optimize(
        &mut self,
        find: impl FnOnce() -> Option<PathBuf>,
        reporter: &Reporter,
    ) -> Result<WasmOpt, Error> {
        let optimized = format!("{}_bg.opt.wasm", self.name);
        let out = self.dir.join(&optimized);
        let ran = if out.exists() {
            WasmOpt::Cached
        } else {
            let Some(wasm_opt) = find() else {
                return Ok(WasmOpt::Missing);
            };
            let tmp = out.with_extension(format!("tmp{}", std::process::id()));
            let mut command = Command::new(&wasm_opt);
            command
                .arg("-Oz")
                .args(WASM_OPT_FEATURES)
                .arg(self.dir.join(&self.wasm))
                .arg("-o")
                .arg(&tmp);
            let line = command_line(&command);
            reporter.emit(&Event::Command(&line));
            let status = command
                .stdin(Stdio::null())
                .status()
                .map_err(|e| Error::Bindgen(format!("`{line}`: {e}")))?;
            if !status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Bindgen(format!("`{line}` failed: {status}")));
            }
            std::fs::rename(&tmp, &out).map_err(|e| Error::Io(out.clone(), e))?;
            WasmOpt::Ran
        };
        self.wasm = optimized;
        Ok(ran)
    }

    fn stage(&self, staging: &Path, files: &mut Vec<Output>) -> Result<Assets, Error> {
        let mut copy = |file: String, stem: &str, ext: &str, kind| -> Result<String, Error> {
            let from = self.dir.join(&file);
            let bytes = std::fs::read(&from).map_err(|e| Error::Io(from.clone(), e))?;
            let name = hashed_name(stem, ext, &bytes);
            let to = staging.join(&name);
            files.push(Output {
                kind,
                path: name.clone().into(),
                size: bytes.len() as u64,
            });
            std::fs::write(&to, bytes).map_err(|e| Error::Io(to, e))?;
            Ok(name)
        };
        let assets = Assets {
            wasm: copy(
                self.wasm.clone(),
                &format!("{}_bg", self.name),
                "wasm",
                OutputKind::Wasm,
            )?,
            js: copy(
                format!("{}.js", self.name),
                &self.name,
                "js",
                OutputKind::Js,
            )?,
        };
        let snippets = self.dir.join("snippets");
        if snippets.is_dir() {
            let (count, size) = copy_dir(&snippets, &staging.join("snippets"))?;
            files.push(Output {
                kind: OutputKind::Assets(count),
                path: "snippets".into(),
                size,
            });
        }
        Ok(assets)
    }
}

fn run_bindgen(metadata: &Metadata, wasm: &Path, reporter: &Reporter) -> Result<Bindgen, Error> {
    let version = metadata
        .packages
        .iter()
        .find(|p| p.name.as_str() == "wasm-bindgen")
        .map(|p| p.version.to_string())
        .ok_or_else(|| Error::Bindgen("the app does not depend on wasm-bindgen".into()))?;
    let name = wasm
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bytes = std::fs::read(wasm).map_err(|e| Error::Io(wasm.to_owned(), e))?;
    let key = hex(&Sha256::new()
        .chain_update(&bytes)
        .chain_update(&version)
        .finalize());
    let dir = metadata
        .target_directory
        .as_std_path()
        .join("trug/bindgen")
        .join(&key[..16]);
    if dir.join(format!("{name}.js")).exists() && dir.join(format!("{name}_bg.wasm")).exists() {
        return Ok(Bindgen::new(dir, name));
    }
    let binary = crate::tools::wasm_bindgen(&version, reporter).map_err(Error::Bindgen)?;
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| Error::Io(tmp.clone(), e))?;
    let mut command = Command::new(&binary);
    command
        .args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            &name,
            "--out-dir",
        ])
        .arg(&tmp)
        .arg(wasm);
    let line = command_line(&command);
    reporter.emit(&Event::Command(&line));
    let status = command
        .stdin(Stdio::null())
        .status()
        .map_err(|e| Error::Bindgen(format!("`{line}`: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(Error::Bindgen(format!("`{line}` failed: {status}")));
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(|e| Error::Io(dir.clone(), e))?;
    Ok(Bindgen::new(dir, name))
}

/// Where trug keeps the tools it downloads: `$TRUG_CACHE_DIR`, else `$XDG_CACHE_HOME/trug`, else `~/.cache/trug`.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TRUG_CACHE_DIR") {
        return dir.into();
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Path::new(&dir).join("trug");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join(".cache/trug")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `stem-<hash of contents>.ext`.
pub fn hashed_name(stem: &str, ext: &str, contents: &[u8]) -> String {
    let digest = Sha256::digest(contents);
    format!("{stem}-{}.{ext}", &hex(&digest)[..16])
}

/// Hashed file names of what the app loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assets {
    pub js: String,
    pub wasm: String,
}

fn loader(assets: &Assets, public_url: &str) -> String {
    let js = format!("{public_url}{}", assets.js);
    let wasm = format!("{public_url}{}", assets.wasm);
    format!(
        "<link rel=\"modulepreload\" href=\"{js}\" crossorigin>\
<link rel=\"preload\" href=\"{wasm}\" as=\"fetch\" type=\"application/wasm\" crossorigin>\
<script type=\"module\">import init from '{js}'; await init({{ module_or_path: '{wasm}' }});</script>"
    )
}

/// The output document: the loader in place of the rust link (or before `</head>` when the document
/// has none), every other data-trug element removed, all other bytes as they were.
fn render(
    html: &str,
    directives: &[Directive],
    plan: &Plan,
    assets: &Assets,
    links: &HashMap<usize, String>,
    public_url: &str,
    extra: Vec<(Range<usize>, String)>,
) -> String {
    let loader = loader(assets, public_url);
    let mut edits: Vec<_> = directives
        .iter()
        .enumerate()
        .map(|(at, d)| {
            let text = if Some(at) == plan.rust {
                loader.clone()
            } else {
                links.get(&at).cloned().unwrap_or_default()
            };
            (d.range.clone(), text)
        })
        .collect();
    edits.extend(extra);
    if plan.rust.is_none() {
        let at = html::head_end(html);
        edits.push((at..at, loader));
    }
    html::replace(html, edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cargo_errors_carry_plain_diagnostics_for_pages_only() {
        let error = Error::Cargo {
            message: "`cargo build` failed: exit status: 101".into(),
            output: "\x1b[1;31merror[E0308]\x1b[0m: mismatched types\n".into(),
        };
        assert_eq!(error.to_string(), "`cargo build` failed: exit status: 101");
        assert_eq!(
            error.detail(),
            "`cargo build` failed: exit status: 101\n\nerror[E0308]: mismatched types"
        );
        assert_eq!(cargo_error("x".into()).detail(), "x");
    }

    #[test]
    fn stderr_keeps_what_follows_the_first_error() {
        let text = "   Compiling a v0.1.0\nerror: failed to run custom build command\n  panic: Undefined variable\n";
        assert_eq!(
            tee_stderr(text.as_bytes(), io::sink()),
            "error: failed to run custom build command\n  panic: Undefined variable\n"
        );
    }

    #[test]
    fn stderr_is_copied_raw_without_progress_redraws() {
        let text = "Building [=>  ] 1/9\rBuilding [==> ] 2/9\r\x1b[K error: boom\r   Compiling a\nerror: failed\nBuilding [=>] 3/9\rnote: x\n";
        let mut out = Vec::new();
        let kept = tee_stderr(text.as_bytes(), &mut out);
        assert_eq!(out, text.as_bytes());
        assert_eq!(kept, "error: failed\nnote: x\n");
    }

    fn assets() -> Assets {
        Assets {
            js: "app-1.js".into(),
            wasm: "app_bg-2.wasm".into(),
        }
    }

    fn plan_for(html: &str) -> (Vec<Directive>, Plan) {
        let directives = html::directives(html);
        let paths = Config::default().paths(Path::new("/app"));
        let plan = Plan::new(&directives, &paths).unwrap();
        (directives, plan)
    }

    #[test]
    fn hashed_names_follow_contents() {
        let a = hashed_name("app", "js", b"one");
        assert_eq!(a, hashed_name("app", "js", b"one"));
        assert_ne!(a, hashed_name("app", "js", b"two"));
        assert!(a.starts_with("app-") && a.ends_with(".js"));
    }

    #[test]
    fn explicit_rust_link_is_replaced_by_the_loader() {
        let html = "<html><head>\r\n<title>x</title>\r\n<link data-trug rel=\"rust\" href=\"app/Cargo.toml\">\r\n</head><body></body></html>";
        let (directives, plan) = plan_for(html);
        assert_eq!(plan.manifest, Path::new("/app/app/Cargo.toml"));
        let out = render(
            html,
            &directives,
            &plan,
            &assets(),
            &HashMap::new(),
            "/",
            vec![],
        );
        assert!(out.starts_with("<html><head>\r\n<title>x</title>\r\n<link rel=\"modulepreload\" href=\"/app-1.js\" crossorigin>"));
        assert!(out.contains(
            "import init from '/app-1.js'; await init({ module_or_path: '/app_bg-2.wasm' });"
        ));
        assert!(out.contains("rel=\"preload\" href=\"/app_bg-2.wasm\" as=\"fetch\""));
        assert!(out.ends_with("</script>\r\n</head><body></body></html>"));
        assert!(!out.contains("data-trug"));
    }

    #[test]
    fn implicit_rust_link_uses_cargo_toml_beside_index() {
        let html = "<head><meta charset=\"utf-8\"></head><body></body>";
        let (directives, plan) = plan_for(html);
        assert_eq!(plan.manifest, Path::new("/app/Cargo.toml"));
        assert!(plan.rust.is_none());
        let out = render(
            html,
            &directives,
            &plan,
            &assets(),
            &HashMap::new(),
            "/base/",
            vec![],
        );
        assert!(out.starts_with(
            "<head><meta charset=\"utf-8\"><link rel=\"modulepreload\" href=\"/base/app-1.js\""
        ));
        assert!(out.ends_with("</script></head><body></body>"));
    }

    #[test]
    fn wasm_opt_output_is_staged_when_found() {
        let tmp = std::env::temp_dir().join(format!("trug-wasm-opt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("app_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        let mut bindgen = Bindgen::new(tmp.clone(), "app".into());
        let silent = Reporter::default();
        assert_eq!(
            bindgen.optimize(|| None, &silent).unwrap(),
            WasmOpt::Missing
        );
        assert_eq!(bindgen.wasm, "app_bg.wasm");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tool = tmp.join("wasm-opt");
            std::fs::write(
                &tool,
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do case $1 in -o) out=$2; shift;; -*) ;; *) in=$1;; esac; shift; done\ncp \"$in\" \"$out\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                bindgen.optimize(|| Some(tool), &silent).unwrap(),
                WasmOpt::Ran
            );
            assert_eq!(bindgen.wasm, "app_bg.opt.wasm");
            assert!(tmp.join("app_bg.opt.wasm").exists());
        }
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn icon_and_copy_dir_are_staged() {
        let tmp = std::env::temp_dir().join(format!("trug-static-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("icons/sub")).unwrap();
        std::fs::create_dir_all(tmp.join("out")).unwrap();
        std::fs::write(tmp.join("icons/a.txt"), "a").unwrap();
        std::fs::write(tmp.join("icons/sub/b.txt"), "b").unwrap();
        std::fs::write(tmp.join("fav.png"), "png").unwrap();
        let html = "<head><base data-trug-public-url><link data-trug rel=\"icon\" type=\"image/png\" href=\"fav.png\"><link data-trug rel=\"copy-dir\" href=\"icons\" data-target-path=\"static/icons\"><link data-trug rel=\"copy-dir\" href=\"icons\"></head>";
        let directives = html::directives(html);
        let paths = Config::default().paths(&tmp);
        let plan = Plan::new(&directives, &paths).unwrap();
        let mut files = Vec::new();
        let links = plan
            .stage_statics(&tmp.join("out"), "/app/", &directives, &mut files)
            .unwrap();
        let kinds: Vec<_> = files.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                OutputKind::Icon,
                OutputKind::Assets(2),
                OutputKind::Assets(2)
            ]
        );
        let mut extra = Vec::new();
        let base = html::public_url_base(html).unwrap();
        extra.push((
            base.range.clone(),
            html::tag(
                "base",
                &base.attrs,
                &["data-trug-public-url"],
                ("href", "/app/"),
            ),
        ));
        let out = render(html, &directives, &plan, &assets(), &links, "/app/", extra);
        let name = hashed_name("fav", "png", b"png");
        assert!(out.starts_with("<head><base href=\"/app/\">"));
        assert!(out.contains(&format!(
            "<link rel=\"icon\" type=\"image/png\" href=\"/app/{name}\">"
        )));
        assert!(!out.contains("data-trug") && !out.contains("icons"));
        assert!(tmp.join("out").join(&name).exists());
        assert_eq!(
            std::fs::read(tmp.join("out/static/icons/sub/b.txt")).unwrap(),
            b"b"
        );
        assert!(tmp.join("out/icons/a.txt").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn rejects_bad_target_paths() {
        assert!(relative_target("../x").is_err());
        assert!(relative_target("/x").is_err());
        assert!(relative_target("a/b").is_ok());
        assert_eq!(normalize_url("/a"), "/a/");
    }

    #[test]
    fn rejects_unsupported_and_duplicate_links() {
        let paths = Config::default().paths(Path::new("/app"));
        let d = html::directives("<link data-trug rel=\"sass\" href=\"a.scss\">");
        assert!(Plan::new(&d, &paths).is_err());
        let d = html::directives("<link data-trug rel=\"rust\"><link data-trug rel=\"rust\">");
        assert!(Plan::new(&d, &paths).is_err());
    }

    #[test]
    fn scss_and_css_links_are_hashed_and_replaced() {
        let root = std::env::temp_dir().join(format!("trug-styles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let staging = root.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            root.join("main.scss"),
            "@use \"pkg:dep-a\" as x;\na { b: c }\n",
        )
        .unwrap();
        std::fs::write(root.join("plain.css"), "p { d: e }\n").unwrap();
        let html = "<head><link data-trug rel=\"scss\" href=\"main.scss\">\n<link data-trug rel=\"css\" href=\"plain.css\"></head>";
        let directives = html::directives(html);
        let paths = Config::default().paths(&root);
        let plan = Plan::new(&directives, &paths).unwrap();
        assert_eq!(plan.styles.len(), 2);
        let query = Query {
            manifest_path: Some(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/app/Cargo.toml"),
            ),
            offline: true,
            ..Query::default()
        };
        let mut outputs = Vec::new();
        let links = stage_styles(
            &plan.styles,
            &query,
            &staging,
            "/pub/",
            false,
            &Reporter::default(),
            &mut outputs,
        )
        .unwrap();
        assert_eq!(outputs.len(), 2);
        let out = render(html, &directives, &plan, &assets(), &links, "/pub/", vec![]);
        let mut files: Vec<_> = std::fs::read_dir(&staging)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files[0].starts_with("main-") && files[1].starts_with("plain-"));
        for name in &files {
            assert!(
                out.contains(&format!("<link rel=\"stylesheet\" href=\"/pub/{name}\">")),
                "{out}"
            );
        }
        let main = std::fs::read_to_string(staging.join(&files[0])).unwrap();
        assert!(main.contains("a {"), "{main}");
        assert_eq!(
            std::fs::read_to_string(staging.join(&files[1])).unwrap(),
            "p { d: e }\n"
        );
        assert!(!out.contains("data-trug") && !out.contains(".scss"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
