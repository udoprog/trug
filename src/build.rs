//! The build pipeline: hooks, cargo, wasm-bindgen, the output index.html, the dist swap.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cargo_metadata::{Message, Metadata};
use sha2::{Digest, Sha256};

use crate::config::{Config, Hook, Paths, Stage};
use crate::dist::Staging;
use crate::html::{self, Directive};
use crate::packages::Query;

pub const WASM_TARGET: &str = "wasm32-unknown-unknown";

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    Packages(crate::packages::Error),
    Html(String),
    Hook(String),
    Cargo(String),
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
            | Error::Cargo(m)
            | Error::Bindgen(m)
            | Error::Css(m) => m.fmt(f),
        }
    }
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
    pub verbose: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub steps: Vec<(&'static str, Duration)>,
    pub dist: PathBuf,
    pub public_url: String,
}

struct Timer<'a> {
    report: &'a mut Report,
    verbose: bool,
}

impl Timer<'_> {
    fn step<T>(
        &mut self,
        name: &'static str,
        f: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        let start = Instant::now();
        let result = f();
        let took = start.elapsed();
        if self.verbose {
            eprintln!("trug: {name}: {:.2}s", took.as_secs_f64());
        }
        self.report.steps.push((name, took));
        result
    }
}

pub fn build(request: &BuildRequest) -> Result<Report, Error> {
    let paths = request.config.paths(&request.root);
    let mut report = Report {
        dist: paths.dist.clone(),
        ..Report::default()
    };
    let mut timer = Timer {
        report: &mut report,
        verbose: request.verbose,
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

    timer.step("hooks", || {
        for hook in &request.config.hooks {
            run_hook(hook, &paths, staging.path())?;
        }
        Ok(())
    })?;

    let mut query = request.query.clone();
    query.manifest_path = Some(plan.manifest.clone());
    query.target = Some(WASM_TARGET.to_owned());

    let links = timer.step("styles", || {
        stage_styles(&plan.styles, &query, staging.path(), &public_url)
    })?;

    let wasm = timer.step("cargo", || cargo_build(&query, request))?;
    let metadata = query.metadata()?;
    let bindgen = timer.step("wasm-bindgen", || run_bindgen(&metadata, &wasm))?;

    timer.step("output", || {
        let assets = bindgen.stage(staging.path())?;
        let mut links = links;
        links.extend(plan.stage_statics(staging.path(), &public_url, &directives)?);
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
        std::fs::write(&index, out).map_err(|e| Error::Io(index, e))
    })?;

    timer.step("swap", || {
        staging.swap().map_err(|e| Error::Io(paths.dist.clone(), e))
    })?;
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
    ) -> Result<HashMap<usize, String>, Error> {
        let mut links = HashMap::new();
        for (at, item) in &self.statics {
            match item {
                Static::CopyDir { from, to } => copy_dir(from, &staging.join(to))?,
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

fn copy_dir(from: &Path, to: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(to).map_err(|e| Error::Io(to.to_owned(), e))?;
    let entries = std::fs::read_dir(from).map_err(|e| Error::Io(from.to_owned(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io(from.to_owned(), e))?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| Error::Io(entry.path(), e))?;
        }
    }
    Ok(())
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
) -> Result<HashMap<usize, String>, Error> {
    let mut links = HashMap::new();
    for style in styles {
        let contents = if style.scss {
            let mut css = crate::CssRequest::new(&style.path);
            css.query = query.clone();
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
        std::fs::write(&to, contents).map_err(|e| Error::Io(to, e))?;
        links.insert(
            style.at,
            format!("<link rel=\"stylesheet\" href=\"{public_url}{name}\">"),
        );
    }
    Ok(links)
}

fn run_hook(hook: &Hook, paths: &Paths, staging: &Path) -> Result<(), Error> {
    match hook.stage {
        Stage::PreBuild => {}
    }
    let status = Command::new(&hook.command)
        .args(&hook.command_arguments)
        .current_dir(&paths.root)
        .env("TRUG_STAGING_DIR", staging)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| Error::Hook(format!("hook {}: {e}", hook.command)))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Hook(format!(
            "hook {} failed: {status}",
            hook.command
        )))
    }
}

fn cargo_build(query: &Query, request: &BuildRequest) -> Result<PathBuf, Error> {
    let manifest = query.manifest_path.as_ref().expect("manifest set by build");
    let mut command = Command::new("cargo");
    command
        .args([
            "build",
            "--message-format=json-render-diagnostics",
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
    ] {
        if on {
            command.arg(flag);
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Cargo(format!("cargo: {e}")))?;
    let stdout = io::BufReader::new(child.stdout.take().expect("piped"));
    let wanted = std::fs::canonicalize(manifest).unwrap_or_else(|_| manifest.clone());
    let mut wasm = None;
    let mut found_cdylib = false;
    for message in Message::parse_stream(stdout).flatten() {
        let Message::CompilerArtifact(artifact) = message else {
            continue;
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
        .map_err(|e| Error::Cargo(format!("cargo: {e}")))?;
    if !status.success() {
        return Err(Error::Cargo(format!("cargo build failed: {status}")));
    }
    wasm.ok_or_else(|| {
        Error::Cargo(format!(
            "{} builds no wasm cdylib or bin; add crate-type = [\"cdylib\"] to its [lib]",
            manifest.display()
        ))
    })
}

/// The files wasm-bindgen produced.
struct Bindgen {
    dir: PathBuf,
    name: String,
}

impl Bindgen {
    fn stage(&self, staging: &Path) -> Result<Assets, Error> {
        let copy = |file: String, stem: &str, ext: &str| -> Result<String, Error> {
            let from = self.dir.join(&file);
            let bytes = std::fs::read(&from).map_err(|e| Error::Io(from.clone(), e))?;
            let name = hashed_name(stem, ext, &bytes);
            let to = staging.join(&name);
            std::fs::write(&to, bytes).map_err(|e| Error::Io(to, e))?;
            Ok(name)
        };
        let snippets = self.dir.join("snippets");
        if snippets.is_dir() {
            copy_dir(&snippets, &staging.join("snippets"))?;
        }
        Ok(Assets {
            js: copy(format!("{}.js", self.name), &self.name, "js")?,
            wasm: copy(
                format!("{}_bg.wasm", self.name),
                &format!("{}_bg", self.name),
                "wasm",
            )?,
        })
    }
}

fn run_bindgen(metadata: &Metadata, wasm: &Path) -> Result<Bindgen, Error> {
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
        return Ok(Bindgen { dir, name });
    }
    let binary = find_bindgen(&version)?;
    let tmp = dir.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| Error::Io(tmp.clone(), e))?;
    let status = Command::new(&binary)
        .args([
            "--target",
            "web",
            "--no-typescript",
            "--out-name",
            &name,
            "--out-dir",
        ])
        .arg(&tmp)
        .arg(wasm)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| Error::Bindgen(format!("{}: {e}", binary.display())))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(Error::Bindgen(format!("wasm-bindgen failed: {status}")));
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(|e| Error::Io(dir.clone(), e))?;
    Ok(Bindgen { dir, name })
}

/// Where trug keeps tools it installs: `$TRUG_CACHE_DIR`, else `$XDG_CACHE_HOME/trug`, else `~/.cache/trug`.
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

fn has_version(binary: &Path, version: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout).trim() == format!("wasm-bindgen {version}")
        })
}

fn find_bindgen(version: &str) -> Result<PathBuf, Error> {
    let on_path = PathBuf::from("wasm-bindgen");
    if has_version(&on_path, version) {
        return Ok(on_path);
    }
    let root = cache_dir().join("wasm-bindgen").join(version);
    let cached = root.join("bin/wasm-bindgen");
    if has_version(&cached, version) {
        return Ok(cached);
    }
    eprintln!(
        "trug: installing wasm-bindgen-cli {version} into {}",
        root.display()
    );
    let mut command = Command::new("cargo");
    command
        .args([
            "install",
            "wasm-bindgen-cli",
            "--locked",
            "--version",
            version,
            "--root",
        ])
        .arg(&root)
        .stdin(Stdio::null());
    let status = command
        .status()
        .map_err(|e| Error::Bindgen(format!("cargo install: {e}")))?;
    if !status.success() || !has_version(&cached, version) {
        return Err(Error::Bindgen(format!(
            "could not install wasm-bindgen-cli {version}"
        )));
    }
    Ok(cached)
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
        let links = plan
            .stage_statics(&tmp.join("out"), "/app/", &directives)
            .unwrap();
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
        let links = stage_styles(&plan.styles, &query, &staging, "/pub/").unwrap();
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
