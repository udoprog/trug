//! Finding the packages in an app's dependency graph that ship Sass.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};

use cargo_metadata::{CargoOpt, DependencyKind, Metadata, MetadataCommand, Package, PackageId};
use sasso::{CanonicalizeContext, FsImporter, Importer};

/// The `[package.metadata.<KEY>]` table a package opts in with.
pub const METADATA_KEY: &str = "trug";

const DEFAULT_DIR: &str = "style";
const DEFAULT_ENTRY: &str = "_index.scss";

/// A package whose stylesheets an app can `@use "pkg:<name>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StylePackage {
    pub name: String,
    pub version: String,
    /// The package's style directory.
    pub dir: PathBuf,
    /// The stylesheet `pkg:<name>` loads, inside `dir`.
    pub entry: PathBuf,
    /// The package's features enabled in this build.
    pub features: BTreeSet<String>,
    /// `[package.metadata.trug.gate]`: each gated stylesheet, resolved to its
    /// path, with the feature that owns it.
    pub gate: BTreeMap<PathBuf, String>,
}

/// How to ask cargo for the app's dependency graph: the same flags the app is built with.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub manifest_path: Option<PathBuf>,
    pub target: Option<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub locked: bool,
    pub offline: bool,
}

#[derive(Debug)]
pub enum Error {
    Cargo(cargo_metadata::Error),
    /// A package's `[package.metadata.trug]` that is malformed.
    Metadata {
        package: String,
        message: String,
    },
    /// A style directory or entry that isn't there.
    Missing {
        package: String,
        path: PathBuf,
    },
    /// Two packages in the graph that answer to the same `pkg:` name.
    Duplicate(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cargo(error) => write!(f, "cargo metadata: {error}"),
            Error::Metadata { package, message } => {
                write!(f, "{package}: [package.metadata.{METADATA_KEY}]: {message}")
            }
            Error::Missing { package, path } => {
                write!(f, "{package}: {} does not exist", path.display())
            }
            Error::Duplicate(name) => write!(
                f,
                "more than one version of {name} ships styles; pkg:{name} would be ambiguous"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<cargo_metadata::Error> for Error {
    fn from(error: cargo_metadata::Error) -> Self {
        Error::Cargo(error)
    }
}

impl Query {
    pub fn metadata(&self) -> Result<Metadata, Error> {
        let mut command = MetadataCommand::new();
        if let Some(path) = &self.manifest_path {
            command.manifest_path(path);
        }
        if self.all_features {
            command.features(CargoOpt::AllFeatures);
        }
        if self.no_default_features {
            command.features(CargoOpt::NoDefaultFeatures);
        }
        if !self.features.is_empty() {
            command.features(CargoOpt::SomeFeatures(self.features.clone()));
        }
        let mut other = Vec::new();
        if let Some(target) = &self.target {
            other.extend(["--filter-platform".to_owned(), target.clone()]);
        }
        if self.locked {
            other.push("--locked".to_owned());
        }
        if self.offline {
            other.push("--offline".to_owned());
        }
        command.other_options(other);
        Ok(command.exec()?)
    }

    pub fn packages(&self) -> Result<Vec<StylePackage>, Error> {
        style_packages(&self.metadata()?)
    }
}

/// The opted-in packages the app (the resolve's root, or every workspace member of a
/// virtual manifest) reaches through normal dependencies, itself included.
pub fn style_packages(metadata: &Metadata) -> Result<Vec<StylePackage>, Error> {
    let Some(resolve) = &metadata.resolve else {
        return Ok(Vec::new());
    };
    let nodes: HashMap<&PackageId, _> = resolve.nodes.iter().map(|node| (&node.id, node)).collect();
    let packages: HashMap<&PackageId, &Package> = metadata
        .packages
        .iter()
        .map(|package| (&package.id, package))
        .collect();

    let mut queue: VecDeque<&PackageId> = match &resolve.root {
        Some(root) => VecDeque::from([root]),
        None => metadata.workspace_members.iter().collect(),
    };
    let mut seen: BTreeSet<&PackageId> = queue.iter().copied().collect();
    let mut found = BTreeMap::new();

    while let Some(id) = queue.pop_front() {
        let Some(node) = nodes.get(id) else { continue };
        if let Some(package) = packages.get(id)
            && let Some(style) = style_package(package, &node.features)?
            && found.insert(style.name.clone(), style).is_some()
        {
            return Err(Error::Duplicate(package.name.to_string()));
        }
        for dep in &node.deps {
            let normal = dep
                .dep_kinds
                .iter()
                .any(|kind| kind.kind == DependencyKind::Normal);
            if normal && seen.insert(&dep.pkg) {
                queue.push_back(&dep.pkg);
            }
        }
    }

    Ok(found.into_values().collect())
}

fn style_package<F: AsRef<str>>(
    package: &Package,
    features: &[F],
) -> Result<Option<StylePackage>, Error> {
    let Some(table) = package.metadata.get(METADATA_KEY) else {
        return Ok(None);
    };
    let name = package.name.to_string();
    let field = |key: &str, default: &str| -> Result<String, Error> {
        match table.get(key) {
            None => Ok(default.to_owned()),
            Some(serde_json::Value::String(value)) => Ok(value.clone()),
            Some(_) => Err(Error::Metadata {
                package: name.clone(),
                message: format!("`{key}` must be a string"),
            }),
        }
    };
    if !table.is_object() {
        return Err(Error::Metadata {
            package: name,
            message: "must be a table".to_owned(),
        });
    }
    let root = package
        .manifest_path
        .parent()
        .map(|dir| dir.as_std_path().to_path_buf())
        .unwrap_or_default();
    let dir = root.join(field("dir", DEFAULT_DIR)?);
    let entry = dir.join(field("entry", DEFAULT_ENTRY)?);
    for path in [&dir, &entry] {
        if !Path::new(path).exists() {
            return Err(Error::Missing {
                package: name,
                path: path.clone(),
            });
        }
    }
    let gate = gate(package, &name, &dir, table.get("gate"))?;
    Ok(Some(StylePackage {
        name: normalize(&name),
        version: package.version.to_string(),
        dir,
        entry,
        features: features
            .iter()
            .map(|feature| feature.as_ref().to_owned())
            .collect(),
        gate,
    }))
}

fn gate(
    package: &Package,
    name: &str,
    dir: &Path,
    table: Option<&serde_json::Value>,
) -> Result<BTreeMap<PathBuf, String>, Error> {
    let error = |message: String| Error::Metadata {
        package: name.to_owned(),
        message,
    };
    let Some(table) = table else {
        return Ok(BTreeMap::new());
    };
    let Some(table) = table.as_object() else {
        return Err(error("`gate` must be a table".to_owned()));
    };
    let fs = FsImporter::new(Vec::new());
    let ctx = CanonicalizeContext {
        from_import: false,
        containing_url: None,
    };
    let mut gate = BTreeMap::new();
    for (file, feature) in table {
        let Some(feature) = feature.as_str() else {
            return Err(error(format!("gate.{file}: must be a feature name")));
        };
        if !package.features.contains_key(feature) {
            return Err(error(format!("gate.{file}: no feature named {feature}")));
        }
        if file.starts_with('/') || file.split('/').any(|part| part == "..") {
            return Err(error(format!(
                "gate.{file}: must stay inside {}",
                dir.display()
            )));
        }
        let found = fs
            .canonicalize(&dir.join(file).to_string_lossy(), &ctx)
            .map_err(|e| error(format!("gate.{file}: {}", e.message)))?
            .ok_or_else(|| {
                error(format!(
                    "gate.{file}: no such stylesheet in {}",
                    dir.display()
                ))
            })?;
        gate.insert(PathBuf::from(found.as_str()), feature.to_owned());
    }
    Ok(gate)
}

/// crates.io treats `-` and `_` alike, so `pkg:` does too.
pub fn normalize(name: &str) -> String {
    name.replace('_', "-")
}
