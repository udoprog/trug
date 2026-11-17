# trug

Builds an app's CSS from its own Sass plus the Sass its crates ship. It uses
[sasso](https://crates.io/crates/sasso) to compile and `cargo metadata` to find
the crates, so it has no resolver of its own and never reads crates.io.

## Opting in

A crate that ships Sass says so in its Cargo.toml:

```toml
[package.metadata.trug]
dir = "style"          # default; the style directory
entry = "_index.scss"  # default; inside dir
```

The style directory must be in the package's `include` so it is published.
Crate names match with `-` and `_` alike.

## `pkg:` URLs

- `@use "pkg:<crate>" as x` loads the crate's entry.
- `@use "pkg:<crate>/<path>"` loads a stylesheet in the crate's style directory
  (partials, `.scss` and index files resolve as in Sass). The path stays inside
  the crate.
- `@use "pkg:<crate>/features"` is a generated module, see below.
- Any other URL resolves relative to the containing file, then the `-I` load paths.

Sasso needs an explicit namespace for the bare `pkg:<crate>` form (`as x` or `as *`).
Inside a crate, relative imports stay in that crate, and a file reached by two
routes is loaded once.

## The features module

`@use "pkg:<crate>/features" as f` exposes the crate's features as enabled in this build:

- `f.$features`: a map of enabled feature names to `true` (empty when none);
- `f.enabled($name)`: whether a feature is enabled;
- `f.$name` and `f.$version`: the crate's name and version.

```scss
@use "pkg:sockel/features" as f;
@if f.enabled("table") {
  .table { ... }
}
```

Features come from the resolve of `cargo metadata`, run with the app's own flags,
so they match what `cargo tree -e features` reports.

## Usage

```
trug css style/app.scss -o dist/app.css -F extra --style compressed
trug packages
```

Both take the app's cargo flags: `--manifest-path`, `--target` (default
`wasm32-unknown-unknown`), `-F/--features`, `--all-features`,
`--no-default-features`, `--locked`, `--offline`. `css` also takes `-I`,
`--style` and `-o` (written atomically, so a failed build keeps the old file).

From Rust, `trug::build_css(&CssRequest)` does the same in-process.

## Building an app (replacing trunk)

`trug build` does what `trunk build` does for an app: it runs the `[[hooks]]`,
builds the crate for `wasm32-unknown-unknown`, runs wasm-bindgen, compiles the
styles, rewrites index.html and writes everything to `dist`. The build goes to a
staging directory beside `dist`, which replaces `dist` in one rename only when
the build succeeded, so a failed build leaves the previous `dist` as it was.

wasm-bindgen is taken at the version the app's `Cargo.lock` has: `wasm-bindgen`
on `PATH` if it is that version, else trug's cache (`$TRUG_CACHE_DIR`, else
`$XDG_CACHE_HOME/trug`, else `~/.cache/trug`), else it is `cargo install`ed there
once. Nothing is downloaded from GitHub.

### Moving from trunk

Three renames, and only these; trug does not read the `data-trunk` forms:

| trunk | trug |
| --- | --- |
| `Trunk.toml` | `Trug.toml` |
| `<link data-trunk rel="...">` | `<link data-trug rel="...">` |
| `<base data-trunk-public-url>` | `<base data-trug-public-url>` |

The `rel` values trug supports are `rust` (at most one; `href` is the crate
directory or Cargo.toml, default `Cargo.toml`), `scss`, `css`, `copy-dir` (with
`data-target-path`) and `icon`. Any other `rel` is an error. Styles and icons are
written under content-hashed names. Sass `rel="scss"` links resolve `pkg:` URLs
like `trug css` does.

### Trug.toml

Every key is optional, unknown keys are errors, and paths are relative to the
Trug.toml.

```toml
[build]
target = "index.html"   # default
dist = "dist"           # default: beside target

[[hooks]]               # run in order before the build
stage = "pre_build"
command = "cargo"
command_arguments = ["run", "-p", "tools"]

[serve]
addresses = ["127.0.0.1"]   # default
port = 8080                 # default
headers = { "Cross-Origin-Opener-Policy" = "same-origin" }  # added to every response

[watch]
watch = ["."]           # default: the directory of Trug.toml
ignore = ["target"]     # glob patterns whose changes are ignored
debounce_ms = 200       # default

[[proxy]]               # repeatable
backend = "http://127.0.0.1:3000/api"
rewrite = "/api"        # optional; the prefix served by trug that the backend path replaces
no_redirect = false     # hand backend redirects to the client
```

### Commands

```
trug build [-c Trug.toml] [--release] [-j N] [--public-url /app/] [-v]
trug serve [--no-watch] [--address ADDR]... [--port N] [build flags]
```

`trug build` takes the app's cargo flags (`--manifest-path`, `--target`, `-F`,
`--all-features`, `--no-default-features`, `--locked`, `--offline`); `-v` prints
each step's time. `--public-url` overrides the `<base data-trug-public-url>` href
and defaults to `/`.

`trug serve` builds, then serves `dist` with the `[serve]` headers, `index.html`
for paths that are not files (SPA routes), and the `[[proxy]]` mounts. Unless
`--no-watch` is given it rebuilds when files under `[watch]` change, one build at
a time; the served `dist` is only ever swapped by a successful build.

## Serve

`trug serve` builds once, serves `dist` and rebuilds when sources change:

```
trug serve --port 8080 --address 127.0.0.1 --no-watch
```

It takes the build's flags and the app's cargo flags. `--address` (repeatable)
and `--port` override `[serve]`; without either it listens on `127.0.0.1:8080`.
Unknown paths without an extension get `index.html`, so SPA routes work. If the
page has a public URL (`<base data-trug-public-url href="/app/">` or
`--public-url`), files are also served under that path.

```toml
[serve]
addresses = ["127.0.0.1"]
port = 8080
headers = { "Cache-Control" = "no-cache" }

[watch]
watch = [".", "../src", "../style"]
ignore = ["style/generated/**"]
debounce_ms = 200
```

- `[serve] headers` are added to every response.
- `[watch] watch` lists the paths to watch, relative to `Trug.toml`; the
  directory of `Trug.toml` by default. `ignore` holds globs of paths whose
  changes are ignored. `dist`, its staging directories, `target` and `.git`
  are always ignored, so a file a `pre_build` hook writes inside a watched
  directory must be listed in `ignore`, or each build triggers the next.
- A change starts one rebuild after `debounce_ms` of quiet. Builds run one at
  a time; changes during a build queue one more.
- Each rebuild goes to a staging directory that replaces `dist` only on
  success. A failed build prints its error and leaves the served page as it
  was.
- Served index pages get a script that listens on `/_trug/reload` (server-sent
  events) and reloads the page after each successful swap, never after a
  failed build.
- `--no-watch` serves the first build only: no watching, no reload script,
  no `/_trug/reload`.

## Proxies

`trug serve` forwards requests under a path prefix to a backend, before static
files and the SPA fallback. Each is a `[[proxy]]` table in `Trug.toml`:

```toml
[[proxy]]
backend = "http://127.0.0.1:8150/api/"
no_redirect = true

[[proxy]]
rewrite = "/ws"
backend = "ws://127.0.0.1:8160/ws"
ws = true
```

- `backend`: the backend URL. Its path is the mount prefix unless `rewrite`
  is set.
- `rewrite`: the prefix trug serves, replaced by the backend's path.
- `no_redirect`: hand backend redirects to the client instead of following them.
- `ws`: proxy WebSocket connections. `backend` must be a `ws://` URL (no
  TLS); the client is upgraded once the backend accepts, and text, binary,
  ping, pong and close frames are piped both ways. A backend that is down
  gives 502. Subprotocol negotiation is not forwarded.
