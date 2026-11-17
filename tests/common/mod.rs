#![allow(dead_code)]

use std::path::PathBuf;

use trug::Query;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

pub fn query(name: &str) -> Query {
    Query {
        manifest_path: Some(fixture(name).join("Cargo.toml")),
        offline: true,
        ..Query::default()
    }
}
