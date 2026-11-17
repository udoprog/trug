//! A sasso importer that adds `pkg:<crate>` URLs to the file system's.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use sasso::{
    CanonicalUrl, CanonicalizeContext, FsImporter, Importer, ImporterError, ImporterResult, Syntax,
};

use crate::packages::{StylePackage, normalize};

const SCHEME: &str = "pkg:";
const FEATURES: &str = "features";

/// Resolves, in order:
///
/// - `pkg:<crate>`: the package's entry stylesheet;
/// - `pkg:<crate>/features`: a generated module with the package's enabled
///   features (`$features`, `enabled($name)`), its `$name` and `$version`;
/// - `pkg:<crate>/<path>`: a stylesheet in the package's style directory,
///   resolved as Sass resolves files (partials, index files);
/// - anything else: relative to the containing file, then the load paths.
///
/// Package files are keyed by their path, so a file reached both as
/// `pkg:<crate>/x` and relatively from inside the package loads once. A file
/// gated on a feature this build leaves off loads as an empty stylesheet,
/// whichever route reaches it.
pub struct PackageImporter {
    packages: BTreeMap<String, StylePackage>,
    gated_off: HashSet<String>,
    fs: FsImporter,
}

impl PackageImporter {
    pub fn new(packages: impl IntoIterator<Item = StylePackage>, load_paths: Vec<PathBuf>) -> Self {
        let packages: BTreeMap<_, _> = packages
            .into_iter()
            .map(|package| (package.name.clone(), package))
            .collect();
        let gated_off = packages
            .values()
            .flat_map(|package| {
                package
                    .gate
                    .iter()
                    .filter(|(_, feature)| !package.features.contains(*feature))
                    .map(|(path, _)| path.to_string_lossy().into_owned())
            })
            .collect();
        Self {
            packages,
            gated_off,
            fs: FsImporter::new(load_paths),
        }
    }

    fn package(&self, name: &str) -> Result<&StylePackage, ImporterError> {
        self.packages.get(&normalize(name)).ok_or_else(|| {
            let known = self.packages.keys().cloned().collect::<Vec<_>>();
            ImporterError {
                message: if known.is_empty() {
                    format!("no package named {name} ships styles (none in this build does)")
                } else {
                    format!(
                        "no package named {name} ships styles; these do: {}",
                        known.join(", ")
                    )
                },
            }
        })
    }

    fn in_package(
        &self,
        path: &Path,
        ctx: &CanonicalizeContext<'_>,
    ) -> Result<Option<CanonicalUrl>, ImporterError> {
        let ctx = CanonicalizeContext {
            from_import: ctx.from_import,
            containing_url: None,
        };
        self.fs.canonicalize(&path.to_string_lossy(), &ctx)
    }
}

impl Importer for PackageImporter {
    fn canonicalize(
        &self,
        url: &str,
        ctx: &CanonicalizeContext<'_>,
    ) -> Result<Option<CanonicalUrl>, ImporterError> {
        let Some(rest) = url.strip_prefix(SCHEME) else {
            return self.fs.canonicalize(url, ctx);
        };
        let (name, path) = match rest.split_once('/') {
            Some((name, path)) => (name, Some(path)),
            None => (rest, None),
        };
        let package = self.package(name)?;
        match path {
            None => self.in_package(&package.entry, ctx),
            Some(FEATURES) => Ok(Some(CanonicalUrl::new(format!(
                "{SCHEME}{}/{FEATURES}",
                package.name
            )))),
            Some(path) if path.starts_with('/') || path.split('/').any(|part| part == "..") => {
                Err(ImporterError {
                    message: format!("{url}: a package path stays inside the package"),
                })
            }
            Some(path) => match self.in_package(&package.dir.join(path), ctx)? {
                Some(found) => Ok(Some(found)),
                None => Err(ImporterError {
                    message: format!("{url}: no such stylesheet in {}", package.dir.display()),
                }),
            },
        }
    }

    fn load(&self, canonical: &CanonicalUrl) -> Result<Option<ImporterResult>, ImporterError> {
        if self.gated_off.contains(canonical.as_str()) {
            return Ok(Some(ImporterResult {
                contents: String::new(),
                syntax: Syntax::Scss,
                source_map_url: None,
            }));
        }
        let Some(rest) = canonical.as_str().strip_prefix(SCHEME) else {
            return self.fs.load(canonical);
        };
        let Some((name, FEATURES)) = rest.split_once('/') else {
            return Ok(None);
        };
        Ok(Some(ImporterResult {
            contents: features_module(self.package(name)?),
            syntax: Syntax::Scss,
            source_map_url: None,
        }))
    }
}

/// The `pkg:<crate>/features` module.
pub fn features_module(package: &StylePackage) -> String {
    let mut out = String::from("@use \"sass:map\";\n");
    let _ = writeln!(out, "$name: {};", quote(&package.name));
    let _ = writeln!(out, "$version: {};", quote(&package.version));
    let entries = package
        .features
        .iter()
        .map(|feature| format!("{}: true", quote(feature)))
        .collect::<Vec<_>>();
    let _ = writeln!(out, "$features: ({});", entries.join(", "));
    out.push_str("@function enabled($feature) {\n  @return map.has-key($features, $feature);\n}\n");
    out
}

fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
