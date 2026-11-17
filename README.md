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
