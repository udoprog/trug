//! How the CLI shows a build: a line per finished step, a live status line on a TTY, warnings,
//! errors and the summary.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trug::build::{OutputKind, Report, WasmOpt};
use trug::{Event, Reporter};

/// Directories larger than this are not gzipped for the summary.
const GZIP_LIMIT: u64 = 64 << 20;
const TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Color {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub color: bool,
    /// Draw a status line for the running step; needs a terminal.
    pub live: bool,
    pub quiet: bool,
    pub verbose: bool,
}

impl Options {
    /// Options for stderr: colour and the status line follow whether it is a terminal, and
    /// `NO_COLOR` turns colour off under `auto`.
    pub fn stderr(color: Color, quiet: bool, verbose: bool) -> Self {
        let tty = io::stderr().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        Self {
            color: match color {
                Color::Auto => tty && !no_color,
                Color::Always => true,
                Color::Never => false,
            },
            live: tty,
            quiet,
            verbose,
        }
    }
}

#[derive(Clone, Copy)]
enum Style {
    Green,
    Cyan,
    Yellow,
    Red,
    Dim,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Green => "\x1b[1;32m",
            Style::Cyan => "\x1b[1;36m",
            Style::Yellow => "\x1b[1;33m",
            Style::Red => "\x1b[1;31m",
            Style::Dim => "\x1b[2m",
        }
    }
}

struct Live {
    verb: &'static str,
    label: &'static str,
    start: Instant,
}

struct State {
    out: Box<dyn Write + Send>,
    live: Option<Live>,
    /// Bumped whenever `live` changes, so a ticker knows its step is over.
    generation: u64,
    drawn: bool,
}

pub struct Term {
    options: Options,
    state: Mutex<State>,
}

impl Term {
    pub fn new(out: Box<dyn Write + Send>, options: Options) -> Arc<Self> {
        Arc::new(Self {
            options,
            state: Mutex::new(State {
                out,
                live: None,
                generation: 0,
                drawn: false,
            }),
        })
    }

    pub fn reporter(self: &Arc<Self>) -> Reporter {
        let term = self.clone();
        Reporter::new(move |event| term.event(event))
    }

    pub fn options(&self) -> Options {
        self.options
    }

    fn paint(&self, style: Style, text: &str) -> String {
        if self.options.color {
            format!("{}{text}\x1b[0m", style.code())
        } else {
            text.to_owned()
        }
    }

    /// Writes `text` as whole lines, clearing the status line first.
    fn write(&self, text: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.drawn {
            let _ = state.out.write_all(b"\r\x1b[2K");
            state.drawn = false;
        }
        let _ = writeln!(state.out, "{text}");
        let _ = state.out.flush();
    }

    /// A cargo-style line: the verb right-aligned in colour, then `text`.
    pub fn status(&self, verb: &str, text: &str) {
        if !self.options.quiet {
            self.write(&self.status_line(verb, Style::Green, text));
        }
    }

    fn status_line(&self, verb: &str, style: Style, text: &str) -> String {
        format!("{} {text}", self.paint(style, &format!("{verb:>12}")))
    }

    pub fn warning(&self, text: &str) {
        self.write(&format!("{} {text}", self.paint(Style::Yellow, "warning:")));
    }

    pub fn error(&self, text: &str) {
        self.write(&format!("{} {text}", self.paint(Style::Red, "error:")));
    }

    fn event(self: &Arc<Self>, event: &Event<'_>) {
        let quiet = self.options.quiet;
        match *event {
            Event::StepStarted { step, child_output } => {
                let (verb, label) = words(step);
                self.set_live(
                    (!quiet && !child_output && self.options.live).then(|| Live {
                        verb,
                        label,
                        start: Instant::now(),
                    }),
                );
            }
            Event::StepFinished { step, took } => {
                self.set_live(None);
                let (verb, label) = words(step);
                self.status(verb, &self.step_text(label, took));
            }
            Event::StepFailed { step, took } => {
                self.set_live(None);
                let (_, label) = words(step);
                let line = self.status_line("Failed", Style::Red, &self.step_text(label, took));
                self.write(&line);
            }
            Event::Command(line) if self.options.verbose => {
                let line = self.status_line("Running", Style::Dim, &format!("`{line}`"));
                self.write(&line);
            }
            Event::Tool { name, path } if self.options.verbose => {
                self.status("Using", &format!("{name} ({})", path.display()));
            }
            Event::Downloading { name, version } if !quiet => {
                let line =
                    self.status_line("Downloading", Style::Cyan, &format!("{name} {version}"));
                self.write(&line);
            }
            Event::Downloaded { name, version } => {
                self.status("Downloaded", &format!("{name} {version}"));
            }
            Event::Installing { name, version } => {
                // cargo install writes to the terminal from here on.
                self.set_live(None);
                if !quiet {
                    let line = self.status_line(
                        "Installing",
                        Style::Cyan,
                        &format!("{name} {version} with cargo install"),
                    );
                    self.write(&line);
                }
            }
            Event::Warning(text) => self.warning(strip_prefix(text, "WARNING: ")),
            Event::Debug(text) if !quiet => {
                let line = format!("{} {text}", self.paint(Style::Cyan, "debug:"));
                self.write(&line);
            }
            Event::BuildFailed(error) => self.error(error),
            _ => {}
        }
    }

    fn step_text(&self, label: &str, took: Duration) -> String {
        format!("{label:<12} {}", self.paint(Style::Dim, &seconds(took)))
    }

    fn set_live(self: &Arc<Self>, live: Option<Live>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.drawn {
            let _ = state.out.write_all(b"\r\x1b[2K");
            let _ = state.out.flush();
            state.drawn = false;
        }
        state.generation += 1;
        let start = live.is_some();
        state.live = live;
        if start {
            let generation = state.generation;
            let term = self.clone();
            std::thread::spawn(move || term.tick(generation));
        }
    }

    /// Redraws the status line until its step is over.
    fn tick(&self, generation: u64) {
        loop {
            std::thread::sleep(TICK);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.generation != generation {
                return;
            }
            let Some(live) = &state.live else {
                return;
            };
            let text = self.status_line(
                live.verb,
                Style::Cyan,
                &format!("{} {:.1}s", live.label, live.start.elapsed().as_secs_f64()),
            );
            let _ = write!(state.out, "\r\x1b[2K{text}");
            let _ = state.out.flush();
            state.drawn = true;
        }
    }

    /// The `Finished` line, after a table of what the build wrote when `files`.
    pub fn finished(&self, report: &Report, release: bool, took: Duration, files: bool) {
        if self.options.quiet {
            return;
        }
        if files {
            self.write(&self.summary(report));
        }
        let profile = if release { "release" } else { "dev" };
        self.status(
            "Finished",
            &format!(
                "{profile} build in {} into {}",
                seconds(took),
                report.dist.display()
            ),
        );
    }

    fn summary(&self, report: &Report) -> String {
        let width = report
            .files
            .iter()
            .map(|f| display_name(&f.path, f.kind).len())
            .max()
            .unwrap_or(0);
        let gzips: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = report
                .files
                .iter()
                .map(|f| scope.spawn(|| gzip_size(&report.dist.join(&f.path), f.size)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().ok().flatten())
                .collect()
        });
        let mut out = String::new();
        for (file, gzip) in report.files.iter().zip(gzips) {
            let name = display_name(&file.path, file.kind);
            let gzip = gzip.map_or_else(|| "-".to_owned(), size);
            let mut line = format!(
                "{:>12} {name:<width$} {:>10}  {}",
                kind(file.kind),
                size(file.size),
                self.paint(Style::Dim, &format!("gzip {gzip:>10}")),
            );
            if let OutputKind::Assets(count) = file.kind {
                let plural = if count == 1 { "" } else { "s" };
                let _ = write!(line, "  {count} file{plural}");
            }
            let _ = writeln!(out, "{line}");
        }
        let wasm = report
            .files
            .iter()
            .find(|f| f.kind == OutputKind::Wasm)
            .map(|f| f.size);
        let opt = match (report.wasm_opt, report.wasm_unoptimized, wasm) {
            (WasmOpt::Ran | WasmOpt::Cached, Some(before), Some(after)) => {
                let cached = if report.wasm_opt == WasmOpt::Cached {
                    " (cached)"
                } else {
                    ""
                };
                format!("ran -Oz{cached}: {} to {}", size(before), size(after))
            }
            (WasmOpt::Ran, ..) => "ran -Oz".to_owned(),
            (WasmOpt::Cached, ..) => "ran -Oz (cached)".to_owned(),
            (WasmOpt::DevBuild, ..) => "skipped: dev build".to_owned(),
            (WasmOpt::Disabled, ..) => "skipped: [build] wasm_opt = false".to_owned(),
            (WasmOpt::Missing, ..) => "skipped: no wasm-opt found".to_owned(),
        };
        let _ = write!(out, "{:>12} {opt}", "wasm-opt");
        out
    }
}

/// The verb and label a step is shown with.
fn words(step: &str) -> (&'static str, &'static str) {
    match step {
        "hooks" => ("Running", "hooks"),
        "styles" => ("Compiling", "styles"),
        "cargo" => ("Building", "wasm"),
        "wasm-bindgen" => ("Generating", "bindings"),
        "wasm-opt" => ("Optimizing", "wasm"),
        "output" => ("Writing", "output"),
        "swap" => ("Replacing", "dist"),
        _ => ("Running", "step"),
    }
}

fn kind(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Html => "html",
        OutputKind::Wasm => "wasm",
        OutputKind::Js => "js",
        OutputKind::Style => "css",
        OutputKind::Icon => "icon",
        OutputKind::Assets(_) => "assets",
    }
}

fn display_name(path: &Path, kind: OutputKind) -> String {
    let mut name = path.display().to_string();
    if matches!(kind, OutputKind::Assets(_)) {
        name.push('/');
    }
    name
}

fn strip_prefix<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.strip_prefix(prefix).unwrap_or(text)
}

fn seconds(took: Duration) -> String {
    format!("{:.2}s", took.as_secs_f64())
}

fn size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.2} MiB", b / (KIB * KIB))
    }
}

/// The gzipped size of the file at `path`, or of each file below it summed.
fn gzip_size(path: &Path, size: u64) -> Option<u64> {
    if path.is_dir() {
        if size > GZIP_LIMIT {
            return None;
        }
        let mut total = 0;
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            let len = entry.metadata().ok()?.len();
            total += gzip_size(&entry.path(), len)?;
        }
        return Some(total);
    }
    let bytes = std::fs::read(path).ok()?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&bytes).ok()?;
    Some(encoder.finish().ok()?.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trug::build::Output;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture(options: Options) -> (Arc<Term>, Buffer) {
        let buffer = Buffer::default();
        (Term::new(Box::new(buffer.clone()), options), buffer)
    }

    fn steps(reporter: &Reporter) {
        for step in ["styles", "cargo"] {
            reporter.emit(&Event::StepStarted {
                step,
                child_output: step == "cargo",
            });
            std::thread::sleep(Duration::from_millis(250));
            reporter.emit(&Event::StepFinished {
                step,
                took: Duration::from_millis(410),
            });
        }
        reporter.emit(&Event::Command("cargo build"));
        reporter.emit(&Event::Warning("WARNING: careful"));
        reporter.emit(&Event::StepStarted {
            step: "wasm-opt",
            child_output: false,
        });
        reporter.emit(&Event::StepFailed {
            step: "wasm-opt",
            took: Duration::from_millis(20),
        });
    }

    #[test]
    fn plain_output_has_aligned_lines_and_no_control_codes() {
        let (term, buffer) = capture(Options::default());
        steps(&term.reporter());
        term.error("`wasm-opt -Oz app.wasm` failed: exit status: 1");
        assert_eq!(
            buffer.text(),
            "   Compiling styles       0.41s\n\
             \x20   Building wasm         0.41s\n\
             warning: careful\n\
             \x20     Failed wasm         0.02s\n\
             error: `wasm-opt -Oz app.wasm` failed: exit status: 1\n"
        );
    }

    #[test]
    fn quiet_keeps_warnings_and_errors_and_verbose_adds_commands() {
        let (term, buffer) = capture(Options {
            quiet: true,
            ..Options::default()
        });
        steps(&term.reporter());
        assert_eq!(
            buffer.text(),
            "warning: careful\n      Failed wasm         0.02s\n"
        );
        let (term, buffer) = capture(Options {
            verbose: true,
            ..Options::default()
        });
        steps(&term.reporter());
        assert!(buffer.text().contains("     Running `cargo build`\n"));
    }

    #[test]
    fn live_status_line_is_cleared_before_other_output() {
        let (term, buffer) = capture(Options {
            live: true,
            ..Options::default()
        });
        steps(&term.reporter());
        let text = buffer.text();
        assert!(text.contains("\r\x1b[2K   Compiling styles 0."), "{text:?}");
        assert!(
            text.contains("\r\x1b[2K   Compiling styles       0.41s\n"),
            "{text:?}"
        );
        assert!(!text.contains("Building wasm 0."), "{text:?}");
    }

    #[test]
    fn colour_wraps_verbs() {
        let (term, buffer) = capture(Options {
            color: true,
            ..Options::default()
        });
        term.status("Finished", "x");
        term.warning("w");
        assert_eq!(
            buffer.text(),
            "\x1b[1;32m    Finished\x1b[0m x\n\x1b[1;33mwarning:\x1b[0m w\n"
        );
    }

    #[test]
    fn summary_lists_files_with_gzip_sizes_and_wasm_opt() {
        let dist = std::env::temp_dir().join(format!("trug-term-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dist);
        std::fs::create_dir_all(dist.join("assets")).unwrap();
        std::fs::write(dist.join("app_bg-1.wasm"), vec![0u8; 2048]).unwrap();
        std::fs::write(dist.join("assets/a.txt"), "a").unwrap();
        let report = Report {
            dist: dist.clone(),
            files: vec![
                Output {
                    kind: OutputKind::Wasm,
                    path: "app_bg-1.wasm".into(),
                    size: 2048,
                },
                Output {
                    kind: OutputKind::Assets(1),
                    path: "assets".into(),
                    size: 1,
                },
            ],
            wasm_opt: WasmOpt::Ran,
            wasm_unoptimized: Some(4096),
            ..Report::default()
        };
        let (term, buffer) = capture(Options::default());
        term.finished(&report, true, Duration::from_millis(1500), true);
        let text = buffer.text();
        let lines: Vec<_> = text.lines().collect();
        assert!(
            lines[0].starts_with("        wasm app_bg-1.wasm    2.0 KiB  gzip "),
            "{text}"
        );
        assert!(
            lines[1].starts_with("      assets assets/              1 B  gzip ")
                && lines[1].ends_with("  1 file"),
            "{text}"
        );
        assert_eq!(lines[2], "    wasm-opt ran -Oz: 4.0 KiB to 2.0 KiB");
        assert_eq!(
            lines[3],
            format!(
                "    Finished release build in 1.50s into {}",
                dist.display()
            )
        );
        std::fs::remove_dir_all(&dist).unwrap();
    }
}
