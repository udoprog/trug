use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use trug::{OutputStyle, Query};

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
            let packages = Query::from(cargo).packages()?;
            let style = match style {
                Style::Expanded => OutputStyle::Expanded,
                Style::Compressed => OutputStyle::Compressed,
            };
            let css = trug::compile(&entry, packages, load_paths, style)?;
            match out {
                Some(out) => trug::write_atomic(&out, &css)?,
                None => print!("{css}"),
            }
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
