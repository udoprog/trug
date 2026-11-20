mod term;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::{Args, Parser, Subcommand, ValueEnum};
use term::Term;
use trug::{CssRequest, OutputStyle, Query};

#[derive(Parser)]
#[command(
    version,
    about = "Builds an app's CSS from its Sass and the Sass its crates ship"
)]
struct Cli {
    /// When to colour output; `auto` colours a terminal unless `NO_COLOR` is set.
    #[arg(long, value_enum, default_value_t = term::Color::Auto, global = true)]
    color: term::Color,
    /// Print only warnings and errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    /// Also print the commands trug runs and the tools it uses.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a stylesheet, with `pkg:<crate>` URLs for every crate that ships styles.
    Css {
        /// The app's stylesheet.
        entry: PathBuf,
        /// Where to write the CSS (atomically); stdout without it.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Extra directories plain `@use` URLs resolve against.
        #[arg(short = 'I', long = "load-path")]
        load_paths: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = Style::Expanded)]
        style: Style,
        /// Append an inline source map with the sources embedded.
        #[arg(long)]
        embed_source_map: bool,
        #[command(flatten)]
        cargo: CargoArgs,
    },
    /// Build the app into dist: hooks, cargo, wasm-bindgen, index.html; dist is replaced only on success.
    Build {
        /// The Trug.toml to build with; `Trug.toml` in the current directory by default, and
        /// without one the defaults (`index.html`, `dist` beside it).
        #[arg(short, long)]
        config: Option<PathBuf>,
        /// The output directory, relative to the current directory; overrides `[build] dist`.
        #[arg(short, long)]
        dist: Option<PathBuf>,
        #[arg(long)]
        release: bool,
        /// Passed to cargo as `--jobs`.
        #[arg(short, long)]
        jobs: Option<u32>,
        /// Prefix of every URL in the output index.html; overrides `<base data-trug-public-url>`
        /// (default `/`).
        #[arg(long)]
        public_url: Option<String>,
        #[command(flatten)]
        cargo: CargoArgs,
    },
    /// Build, serve dist with the `[serve]` headers and index.html for SPA routes, and rebuild on changes.
    Serve {
        /// Serve the first build without rebuilding when sources change.
        #[arg(long)]
        no_watch: bool,
        /// Glob of paths whose changes are ignored, relative to `Trug.toml`; repeatable. Adds to
        /// `[watch] ignore`.
        #[arg(long = "ignore", value_name = "GLOB")]
        ignore: Vec<String>,
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(short, long)]
        dist: Option<PathBuf>,
        #[arg(long)]
        release: bool,
        #[arg(short, long)]
        jobs: Option<u32>,
        #[arg(long)]
        public_url: Option<String>,
        /// Address to listen on; repeatable. Overrides `[serve] addresses`.
        #[arg(long = "address")]
        addresses: Vec<String>,
        /// Overrides `[serve] port`; 8080 by default.
        #[arg(long)]
        port: Option<u16>,
        #[command(flatten)]
        cargo: CargoArgs,
    },
    /// Update trug to the latest GitHub release.
    Update,
    /// List the crates that ship styles, with their enabled features.
    Packages {
        #[command(flatten)]
        cargo: CargoArgs,
    },
}

/// The flags the app is built with, so features resolve the same way.
#[derive(Args)]
struct CargoArgs {
    #[arg(long)]
    manifest_path: Option<PathBuf>,
    #[arg(long, default_value = "wasm32-unknown-unknown")]
    target: String,
    #[arg(short = 'F', long, value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long)]
    locked: bool,
    #[arg(long)]
    offline: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Style {
    Expanded,
    Compressed,
}

impl From<CargoArgs> for Query {
    fn from(args: CargoArgs) -> Self {
        Query {
            manifest_path: args.manifest_path,
            target: Some(args.target),
            features: args.features,
            all_features: args.all_features,
            no_default_features: args.no_default_features,
            locked: args.locked,
            offline: args.offline,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let options = term::Options::stderr(cli.color, cli.quiet, cli.verbose);
    let term = Term::new(Box::new(std::io::stderr()), options);
    match run(cli.command, &term) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            term.error(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, term: &Arc<Term>) -> Result<(), Box<dyn std::error::Error>> {
    let quiet = term.options().quiet;
    match command {
        Command::Css {
            entry,
            out,
            load_paths,
            style,
            embed_source_map,
            cargo,
        } => {
            let style = match style {
                Style::Expanded => OutputStyle::Expanded,
                Style::Compressed => OutputStyle::Compressed,
            };
            let css = trug::build_css(&CssRequest {
                query: cargo.into(),
                entry,
                load_paths,
                style,
                source_map: embed_source_map,
                reporter: term.reporter(),
            })?;
            match out {
                Some(out) => trug::write_atomic(&out, &css)?,
                None => print!("{css}"),
            }
        }
        Command::Build {
            config,
            dist,
            release,
            jobs,
            public_url,
            cargo,
        } => {
            let (config, root) = load_config(config, dist)?;
            let start = Instant::now();
            let report = trug::build::build(&trug::build::BuildRequest {
                config,
                root,
                query: cargo.into(),
                release,
                jobs,
                public_url,
                quiet,
                color: term.options().color,
                reporter: term.reporter(),
            })?;
            term.finished(&report, release, start.elapsed(), true);
        }
        Command::Serve {
            config,
            dist,
            release,
            jobs,
            public_url,
            addresses,
            port,
            no_watch,
            ignore,
            cargo,
        } => {
            let (config, root) = load_config(config, dist)?;
            let serve = config.serve.clone();
            let proxies = config.proxy.clone();
            let dist = config.paths(&root).dist;
            let request = trug::build::BuildRequest {
                config,
                root,
                query: cargo.into(),
                release,
                jobs,
                public_url,
                quiet,
                color: term.options().color,
                reporter: term.reporter(),
            };
            let start = Instant::now();
            let report = trug::build::build(&request)?;
            term.finished(&report, release, start.elapsed(), true);
            let addresses = if addresses.is_empty() {
                serve.addresses.unwrap_or_else(|| vec!["127.0.0.1".into()])
            } else {
                addresses
            };
            let port = port.or(serve.port).unwrap_or(8080);
            let mut site = trug::serve::Site::new(dist, &serve.headers)?
                .with_proxies(&proxies)?
                .with_public_url(&report.public_url)
                .with_reporter(term.reporter());
            if !no_watch {
                site = site.with_reload();
            }
            let proxied = proxies
                .iter()
                .map(|p| {
                    let mount = trug::proxy::Proxy::new(p)?.mount().to_owned();
                    Ok((mount, p.backend.clone()))
                })
                .collect::<Result<Vec<_>, trug::proxy::Error>>()?;
            let root = request.root.clone();
            let watcher = if no_watch {
                None
            } else {
                Some(start_watch(request, ignore, site.clone(), term.clone())?)
            };
            let mut watching = watcher.as_ref().map_or_else(Vec::new, |(_, paths)| {
                paths
                    .iter()
                    .map(|p| shown_path(&root, p))
                    .collect::<Vec<_>>()
            });
            // The app's own paths first, then the crates' style dirs.
            watching.sort_by_key(|p| std::path::Path::new(p).is_absolute());
            tokio::runtime::Runtime::new()?.block_on(async {
                let listeners = trug::serve::bind(&addresses, port).await?;
                let urls = listeners
                    .iter()
                    .map(|l| Ok(format!("http://{}{}", l.local_addr()?, report.public_url)))
                    .collect::<std::io::Result<Vec<_>>>()?;
                term.banner(&urls, &proxied, &watching);
                tokio::select! {
                    result = site.serve(listeners) => result?,
                    _ = tokio::signal::ctrl_c() => {}
                }
                Ok::<_, Box<dyn std::error::Error>>(())
            })?;
        }
        Command::Update => trug::update::update(&term.reporter())?,
        Command::Packages { cargo } => {
            for package in Query::from(cargo).packages()? {
                let features = package.features.into_iter().collect::<Vec<_>>();
                println!(
                    "{} {} {} [{}]",
                    package.name,
                    package.version,
                    package.dir.display(),
                    features.join(", ")
                );
            }
        }
    }
    Ok(())
}

fn load_config(
    config: Option<PathBuf>,
    dist: Option<PathBuf>,
) -> Result<(trug::config::Config, PathBuf), Box<dyn std::error::Error>> {
    let path = config.unwrap_or_else(|| PathBuf::from(trug::config::FILE_NAME));
    let (mut config, root) = if path.exists() {
        let root = std::path::absolute(&path)?
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        (trug::config::Config::load(&path)?, root)
    } else {
        (trug::config::Config::default(), std::env::current_dir()?)
    };
    if let Some(dist) = dist {
        config.build.dist = Some(std::path::absolute(dist)?);
    }
    Ok((config, root))
}

/// `path` relative to `root` when below it, `.` for the root itself.
fn shown_path(root: &std::path::Path, path: &std::path::Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// Starts rebuilding on changes; returns the watcher and the paths it watches.
fn start_watch(
    request: trug::build::BuildRequest,
    ignore: Vec<String>,
    site: trug::serve::Site,
    term: Arc<Term>,
) -> Result<(trug::watch::Watcher, Vec<PathBuf>), Box<dyn std::error::Error>> {
    let root = request.root.clone();
    let mut settings = request.config.watch.clone();
    settings.ignore.extend(ignore);
    let paths = request.config.paths(&root);
    let mut watch: Vec<PathBuf> = match &settings.watch {
        Some(list) => list.iter().map(|p| root.join(p)).collect(),
        None => vec![root.clone()],
    };
    let mut ignored = vec![paths.dist.clone(), root.join("target"), root.join(".git")];
    match request.query.metadata().and_then(|metadata| {
        let target = metadata.target_directory.clone().into_std_path_buf();
        trug::packages::style_packages(&metadata).map(|packages| (packages, target))
    }) {
        Ok((packages, target)) => {
            watch.extend(packages.into_iter().map(|p| p.dir));
            ignored.push(target);
        }
        Err(error) => term.warning(&format!("not watching package styles: {error}")),
    }
    watch.retain(|path| path.exists());
    watch.sort();
    watch.dedup();
    let ignore = trug::watch::Ignore::new(&root, &settings.ignore, ignored, Some(&paths.dist))?;
    let debounce = settings.debounce_ms.map_or(
        trug::watch::DEFAULT_DEBOUNCE,
        std::time::Duration::from_millis,
    );
    let watcher = trug::watch::Watcher::start(
        trug::watch::WatchRequest {
            paths: watch.clone(),
            ignore,
            debounce,
            reporter: term.reporter(),
        },
        {
            let term = term.clone();
            let site = site.clone();
            move || {
                let start = Instant::now();
                match trug::build::build(&request) {
                    Ok(report) => {
                        term.finished(&report, request.release, start.elapsed(), false);
                        Ok(())
                    }
                    Err(error) => {
                        site.notify_error(&error.detail());
                        Err(error)
                    }
                }
            }
        },
        move || term.reloaded(site.notify_reload()),
    )?;
    Ok((watcher, watch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_ignore_is_repeatable() {
        let cli = Cli::try_parse_from(["trug", "serve", "--ignore", "a/**", "--ignore", "*.wgsl"])
            .unwrap();
        let Command::Serve { ignore, .. } = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(ignore, ["a/**", "*.wgsl"]);
    }

    #[test]
    fn output_flags_are_global() {
        let cli = Cli::try_parse_from(["trug", "build", "-v", "--color", "never"]).unwrap();
        assert!(cli.verbose && !cli.quiet && cli.color == term::Color::Never);
        let cli = Cli::try_parse_from(["trug", "-q", "serve"]).unwrap();
        assert!(cli.quiet);
        assert!(Cli::try_parse_from(["trug", "build", "-q", "-v"]).is_err());
    }
}
