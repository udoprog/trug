mod common;

use common::{fixture, query};
use trug::packages::{Error, normalize};

#[test]
fn opted_in_packages_only() {
    let packages = query("app").packages().unwrap();
    let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["custom-b", "dep-a"]);
}

#[test]
fn defaults_and_custom_dir_entry() {
    let packages = query("app").packages().unwrap();
    let a = packages.iter().find(|p| p.name == "dep-a").unwrap();
    assert_eq!(a.version, "1.0.0");
    assert_eq!(a.dir, fixture("dep-a").join("style"));
    assert_eq!(a.entry, fixture("dep-a").join("style/_index.scss"));
    let b = packages.iter().find(|p| p.name == "custom-b").unwrap();
    assert_eq!(b.dir, fixture("custom_b").join("css"));
    assert_eq!(b.entry, fixture("custom_b").join("css/main.scss"));
}

#[test]
fn virtual_manifest_walks_members() {
    let packages = query("ws").packages().unwrap();
    let names: Vec<_> = packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["dep-a"]);
}

#[test]
fn two_versions_are_an_error() {
    let error = query("dup").packages().unwrap_err();
    assert!(
        matches!(&error, Error::Duplicate(name) if name == "dep-a"),
        "{error}"
    );
}

#[test]
fn names_normalise() {
    assert_eq!(normalize("custom_b"), "custom-b");
    assert_eq!(normalize("dep-a"), "dep-a");
}

#[test]
fn features_follow_the_app_flags() {
    let features = |q: trug::Query| {
        let packages = q.packages().unwrap();
        let a = packages.into_iter().find(|p| p.name == "dep-a").unwrap();
        a.features.into_iter().collect::<Vec<_>>()
    };
    assert_eq!(features(query("app")), ["base"]);

    let mut on = query("app");
    on.features = vec!["extra".to_owned()];
    assert_eq!(features(on), ["base", "extra"]);

    let mut all = query("app");
    all.all_features = true;
    assert_eq!(features(all), ["base", "extra"]);

    let mut none = query("ws");
    none.no_default_features = true;
    assert_eq!(features(none), ["base", "default"]);

    assert_eq!(features(query("ws")), ["base", "default", "extra"]);
}
