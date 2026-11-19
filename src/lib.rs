//! [<img alt="github" src="https://img.shields.io/badge/github-udoprog/trug-8da0cb?style=for-the-badge&logo=github" height="20">](https://github.com/udoprog/trug)
//! [<img alt="crates.io" src="https://img.shields.io/crates/v/trug.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://crates.io/crates/trug)
//! [<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-trug-66c2a5?style=for-the-badge&logoColor=white&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K" height="20">](https://docs.rs/trug)
//! Builds an app's CSS from its own Sass plus the Sass its crates ship. It uses
//! [sasso](https://crates.io/crates/sasso) to compile and `cargo metadata` to find
//! the crates, so it has no resolver of its own and never reads crates.io.
//!
//! <br>
//!
//! ## Opting in
//!
//! A crate that ships Sass says so in its Cargo.toml:
//!
//! ```toml
//! [package.metadata.trug]
//! dir = "style"          # default; the style directory
//! entry = "_index.scss"  # default; inside dir
//! ```
//!
//! The style directory must be in the package's `include` so it is published.
//! Crate names match with `-` and `_` alike.
//!
//! A crate can tie a stylesheet to the Cargo feature that owns it:
//!
//! ```toml
//! [package.metadata.trug.gate]
//! buttons = "button"
//! ```
//!
//! Each key names a file in the style directory as `pkg:<crate>/<key>` would
//! (`buttons` is `_buttons.scss` or `buttons.scss`). When the feature is off in
//! the app's build, that file loads as an empty stylesheet, through `pkg:` and
//! through relative loads alike, so the Sass needs no conditional `@use` and still
//! compiles on its own. A gated file should only emit CSS, since its members are
//! gone when it is off. An unknown feature or a key that names no file is an error.
//!
//! <br>
//!
//! ## `pkg:` URLs
//!
//! - `@use "pkg:<crate>" as x` loads the crate's entry.
//! - `@use "pkg:<crate>/<path>"` loads a stylesheet in the crate's style directory
//!   (partials, `.scss` and index files resolve as in Sass). The path stays inside
//!   the crate.
//! - `@use "pkg:<crate>/features"` is a generated module, see below.
//! - Any other URL resolves relative to the containing file, then the `-I` load paths.
//!
//! Sasso needs an explicit namespace for the bare `pkg:<crate>` form (`as x` or `as *`).
//! Inside a crate, relative imports stay in that crate, and a file reached by two
//! routes is loaded once.
//!
//! <br>
//!
//! ## The features module
//!
//! `@use "pkg:<crate>/features" as f` exposes the crate's features as enabled in this build:
//!
//! - `f.$features`: a map of enabled feature names to `true` (empty when none);
//! - `f.enabled($name)`: whether a feature is enabled;
//! - `f.$name` and `f.$version`: the crate's name and version.
//!
//! ```scss
//! @use "pkg:sockel/features" as f;
//! @if f.enabled("table") {
//!   .table { ... }
//! }
//! ```
//!
//! Features come from the resolve of `cargo metadata`, run with the app's own flags,
//! so they match what `cargo tree -e features` reports.
//!
//! <br>
//!
//! ## Usage
//!
//! ```shell
//! trug css style/app.scss -o dist/app.css -F extra --style compressed
//! trug packages
//! ```
//!
//! Both take the app's cargo flags: `--manifest-path`, `--target` (default
//! `wasm32-unknown-unknown`), `-F/--features`, `--all-features`,
//! `--no-default-features`, `--locked`, `--offline`. `css` also takes `-I`,
//! `--style`, `-o` (written atomically, so a failed build keeps the old file) and
//! `--embed-source-map`, which appends an inline source map with the sources in it.
//!
//! From Rust, `trug::build_css(&CssRequest)` does the same in-process.
//!
//! <br>
//!
//! ## Building an app (replacing trunk)
//!
//! `trug build` does what `trunk build` does for an app: it runs the `[[hooks]]`,
//! builds the crate for `wasm32-unknown-unknown`, runs wasm-bindgen, compiles the
//! styles, rewrites index.html and writes everything to `dist`, which is `dist`
//! beside index.html unless `[build] dist` or `-d` names another. The build goes to
//! a staging directory beside `dist`, which replaces `dist` in one rename only when
//! the build succeeded, so a failed build leaves the previous `dist` as it was.
//!
//! The app's wasm and JS go to the top of `dist` as `<name>_bg-<hash>.wasm` and
//! `<name>-<hash>.js`. `<name>` is the stem of the wasm cargo builds (for a cdylib,
//! the lib name with `-` as `_`) and `<hash>` is the first 16 hex digits of the
//! file's sha256.
//!
//! trug fetches the tools it needs into its cache (`$TRUG_CACHE_DIR`, else
//! `$XDG_CACHE_HOME/trug`, else `~/.cache/trug`) once, so later builds need no
//! network:
//!
//! - wasm-bindgen, at the version the app's `Cargo.lock` has: `wasm-bindgen` on
//!   `PATH` if it is that version, else the cache, else the prebuilt release from
//!   [github.com/wasm-bindgen/wasm-bindgen] into `wasm-bindgen/<version>/`, checked
//!   against its published sha256. Without a prebuilt for the host, or when the
//!   download fails, it is `cargo install`ed there instead.
//! - wasm-opt, which `--release` builds run as `wasm-opt -Oz` on the wasm unless
//!   `[build] wasm_opt = false`; dev builds never run it. It is
//!   `wasm-opt` on `PATH` if there is one, else binaryen's prebuilt release (a
//!   version pinned in trug) from [github.com/WebAssembly/binaryen] into
//!   `binaryen/<version>/`, checked against its sha256. Without one trug warns and
//!   stages the wasm unoptimized.
//!
//! [github.com/wasm-bindgen/wasm-bindgen]: https://github.com/wasm-bindgen/wasm-bindgen/releases
//! [github.com/WebAssembly/binaryen]: https://github.com/WebAssembly/binaryen/releases
//!
//! <br>
//!
//! ### Moving from trunk
//!
//! Three renames, and only these; trug does not read the `data-trunk` forms:
//!
//! | trunk | trug |
//! | --- | --- |
//! | `Trunk.toml` | `Trug.toml` |
//! | `<link data-trunk rel="...">` | `<link data-trug rel="...">` |
//! | `<base data-trunk-public-url>` | `<base data-trug-public-url>` |
//!
//! The `rel` values trug supports are `rust` (at most one; `href` is the crate
//! directory or Cargo.toml, default `Cargo.toml`), `scss`, `css`, `copy-dir`
//! (`data-target-path` is optional and defaults to the directory's own name) and
//! `icon`. Any other `rel` is an error. Styles and icons are written under
//! content-hashed names. Sass `rel="scss"` links resolve `pkg:` URLs
//! like `trug css` does and, like trunk, embed a source map with the sources in dev
//! builds and none with `--release`.
//!
//! trug's output differs from trunk's:
//!
//! - `index.html` has no SRI `integrity` attributes, no `window.wasmBindings` and no
//!   `TrunkApplicationStarted` event.
//! - The wasm is named `<name>_bg-HASH.wasm`, where trunk writes `<name>-HASH_bg.wasm`.
//! - There is no `.trunk-lock`.
//!
//! <br>
//!
//! ### Trug.toml
//!
//! Every key is optional, unknown keys are errors, and paths are relative to the
//! Trug.toml.
//!
//! ```toml
//! [build]
//! target = "index.html"   # default
//! dist = "dist"           # default: beside target
//! wasm_opt = true         # default; false skips wasm-opt on --release builds
//!
//! [[hooks]]               # run in order before the build
//! stage = "pre_build"
//! command = "cargo"
//! command_arguments = ["run", "-p", "tools"]
//!
//! [serve]
//! addresses = ["127.0.0.1"]   # default
//! port = 8080                 # default
//! headers = { "Cross-Origin-Opener-Policy" = "same-origin" }  # added to every response
//!
//! [watch]
//! watch = ["."]           # default: the directory of Trug.toml
//! ignore = ["target"]     # glob patterns whose changes are ignored
//! debounce_ms = 200       # default
//!
//! [[proxy]]               # repeatable
//! backend = "http://127.0.0.1:3000/api"
//! rewrite = "/api"        # optional; the prefix served by trug that the backend path replaces
//! no_redirect = false     # hand backend redirects to the client
//! ```
//!
//! <br>
//!
//! ### Commands
//!
//! ```shell
//! trug build [-c Trug.toml] [-d DIR] [--release] [-j N] [--public-url /app/] [-v]
//! trug serve [--no-watch] [--ignore GLOB]... [--address ADDR]... [--port N] [build flags]
//! ```
//!
//! `trug build` takes the app's cargo flags (`--manifest-path`, `--target`, `-F`,
//! `--all-features`, `--no-default-features`, `--locked`, `--offline`); `-v` prints
//! each step's time. `--public-url` overrides the `<base data-trug-public-url>` href
//! and defaults to `/`. `-d`/`--dist DIR` overrides `[build] dist` with a path
//! relative to the current directory; it is still replaced only on success.
//!
//! <br>
//!
//! ## Serve
//!
//! `trug serve` builds once, serves `dist` and rebuilds when sources change:
//!
//! ```shell
//! trug serve --port 8080 --address 127.0.0.1 --no-watch
//! ```
//!
//! It takes the build's flags and the app's cargo flags. `--address` (repeatable)
//! and `--port` override `[serve]`; without either it listens on `127.0.0.1:8080`.
//! Unknown paths without an extension get `index.html`, so SPA routes work. If the
//! page has a public URL (`<base data-trug-public-url href="/app/">` or
//! `--public-url`), files are also served under that path.
//!
//! ```toml
//! [serve]
//! addresses = ["127.0.0.1"]
//! port = 8080
//! headers = { "Cache-Control" = "no-cache" }
//!
//! [watch]
//! watch = [".", "../src", "../style"]
//! ignore = ["style/generated/**"]
//! debounce_ms = 200
//! ```
//!
//! - `[serve] headers` are added to every response.
//! - `[watch] watch` lists the paths to watch, relative to `Trug.toml`; the
//!   directory of `Trug.toml` by default. `ignore` holds globs of paths whose
//!   changes are ignored. `dist`, its staging directories, `target` and `.git`
//!   are always ignored. A write that leaves a file's content as it was does not
//!   start a rebuild, so a `pre_build` hook that regenerates identical output
//!   inside a watched directory settles after one build. A hook whose output
//!   changes on every run (a timestamp, say) still rebuilds each time; list such
//!   files in `ignore`.
//! - A change starts one rebuild after `debounce_ms` of quiet. Builds run one at
//!   a time; changes during a build queue one more.
//! - Each rebuild goes to a staging directory that replaces `dist` only on
//!   success. A failed build prints its error and leaves the served page as it
//!   was.
//! - Served index pages get a script that listens on `/_trug/reload` (server-sent
//!   events) and reloads the page after each successful swap, never after a
//!   failed build.
//! - `--no-watch` serves the first build only: no watching, no reload script,
//!   no `/_trug/reload`.
//! - `--ignore GLOB` (repeatable) adds to `[watch] ignore` for one run, so
//!   `trug serve --ignore 'src/shaders/**'` keeps shader edits from rebuilding.
//!
//! <br>
//!
//! ## Proxies
//!
//! `trug serve` forwards requests under a path prefix to a backend, before static
//! files and the SPA fallback. Each is a `[[proxy]]` table in `Trug.toml`:
//!
//! ```toml
//! [[proxy]]
//! backend = "http://127.0.0.1:8150/api/"
//! no_redirect = true
//!
//! [[proxy]]
//! rewrite = "/ws"
//! backend = "ws://127.0.0.1:8160/ws"
//! ws = true
//! ```
//!
//! - `backend`: the backend URL. Its path is the mount prefix unless `rewrite`
//!   is set.
//! - `rewrite`: the prefix trug serves, replaced by the backend's path.
//! - `no_redirect`: hand backend redirects to the client instead of following them.
//! - `ws`: proxy WebSocket connections. `backend` must be a `ws://` URL (no
//!   TLS); the client is upgraded once the backend accepts, and text, binary,
//!   ping, pong and close frames are piped both ways. The client's request
//!   headers (cookies, `Origin`, ...) go with the backend handshake, as for
//!   HTTP. A backend that refuses the upgrade passes its response through; one
//!   that is down gives 502. Subprotocol negotiation is not forwarded.

pub mod build;
pub mod config;
pub mod dist;
pub mod html;
pub mod importer;
pub mod packages;
pub mod proxy;
pub mod serve;
mod tools;
pub mod watch;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;

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

/// Everything the CSS step needs: the cargo flags the app is built with, the
/// app's stylesheet, extra load paths and the output style.
#[derive(Clone, Debug)]
pub struct CssRequest {
    pub query: Query,
    pub entry: PathBuf,
    pub load_paths: Vec<PathBuf>,
    pub style: OutputStyle,
    /// Append an inline source map, with the sources embedded.
    pub source_map: bool,
}

impl CssRequest {
    pub fn new(entry: impl Into<PathBuf>) -> Self {
        Self {
            query: Query::default(),
            entry: entry.into(),
            load_paths: Vec::new(),
            style: OutputStyle::Expanded,
            source_map: false,
        }
    }
}

/// Discovers the packages that ship styles and compiles the request's
/// stylesheet with them available as `pkg:` URLs.
pub fn build_css(request: &CssRequest) -> Result<String, Error> {
    let packages = request.query.packages()?;
    compile(
        &request.entry,
        packages,
        request.load_paths.clone(),
        request.style,
        request.source_map,
    )
}

/// Compiles the stylesheet at `entry` with `packages` available as `pkg:` URLs.
pub fn compile(
    entry: &Path,
    packages: Vec<StylePackage>,
    load_paths: Vec<PathBuf>,
    style: OutputStyle,
    source_map: bool,
) -> Result<String, Error> {
    let entry = std::path::absolute(entry).map_err(|e| Error::Io(entry.to_owned(), e))?;
    let source = std::fs::read_to_string(&entry).map_err(|e| Error::Io(entry.clone(), e))?;
    let importer = PackageImporter::new(packages, load_paths);
    let url = entry.to_string_lossy();
    let options = sasso::Options::new()
        .with_importer(&importer)
        .with_url(&url)
        .with_style(style);
    if !source_map {
        return sasso::compile(&source, &options).map_err(Error::Sass);
    }
    let options = options.with_source_map_include_sources(true);
    let result = sasso::compile_with_source_map(&source, &options).map_err(Error::Sass)?;
    let map = base64::engine::general_purpose::STANDARD.encode(result.source_map.to_json());
    let mut css = result.css;
    if !css.is_empty() && !css.ends_with('\n') {
        css.push('\n');
    }
    css.push_str(&format!(
        "/*# sourceMappingURL=data:application/json;charset=utf-8;base64,{map} */\n"
    ));
    Ok(css)
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
