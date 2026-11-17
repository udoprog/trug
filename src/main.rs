use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use trug::{CssRequest, OutputStyle, Query};

#[derive(Parser)]
#[command(
    version,
    about = "Builds an app's CSS from its Sass and the Sass its crates ship"
)]
struct Cli {
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
        #[command(flatten)]
        cargo: CargoArgs,
    },
    /// Build the app into dist: hooks, cargo, wasm-bindgen, index.html; dist is replaced only on success.
    Build {
        /// The Trug.toml to build with; `Trug.toml` in the current directory by default, and
        /// without one the defaults (`index.html`, `dist` beside it).
        #[arg(short, long)]
        config: Option<PathBuf>,
        #[arg(long)]
        release: bool,
        /// Passed to cargo as `--jobs`.
        #[arg(short, long)]
        jobs: Option<u32>,
        /// Prefix of every URL in the output index.html.
        #[arg(long, default_value = "/")]
        public_url: String,
        /// Print each step's time.
        #[arg(short, long)]
        verbose: bool,
        #[command(flatten)]
        cargo: CargoArgs,
    },
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
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("trug: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Css {
            entry,
            out,
            load_paths,
            style,
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
            })?;
            match out {
                Some(out) => trug::write_atomic(&out, &css)?,
                None => print!("{css}"),
            }
        }
        Command::Build {
            config,
            release,
            jobs,
            public_url,
            verbose,
            cargo,
        } => {
            let path = config.unwrap_or_else(|| PathBuf::from(trug::config::FILE_NAME));
            let (config, root) = if path.exists() {
                let root = std::path::absolute(&path)?
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_default();
                (trug::config::Config::load(&path)?, root)
            } else {
                (trug::config::Config::default(), std::env::current_dir()?)
            };
            let mut public_url = public_url;
            if !public_url.ends_with('/') {
                public_url.push('/');
            }
            let report = trug::build::build(&trug::build::BuildRequest {
                config,
                root,
                query: cargo.into(),
                release,
                jobs,
                public_url,
                verbose,
            })?;
            eprintln!("trug: built {}", report.dist.display());
        }
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
