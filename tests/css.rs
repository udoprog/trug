mod common;

use common::{fixture, query};
use trug::{CssRequest, Error};

fn request(source: &str, tag: &str) -> CssRequest {
    let dir = std::env::temp_dir().join(format!("trug-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("app.scss");
    std::fs::write(&entry, source).unwrap();
    let mut request = CssRequest::new(entry);
    request.query = query("app");
    request
}

fn build(source: &str, tag: &str) -> Result<String, Error> {
    trug::build_css(&request(source, tag))
}

#[test]
fn entry_by_crate_name() {
    let css = build("@use \"pkg:dep-a\" as x;", "entry").unwrap();
    assert!(css.contains(".a-base"), "{css}");
}

#[test]
fn custom_entry_and_underscore_name() {
    let css = build("@use \"pkg:custom_b\" as x;", "custom").unwrap();
    assert!(css.contains(".b-main"), "{css}");
    assert!(css.contains("custom-b 0.3.0"), "{css}");
}

#[test]
fn partial_with_and_without_underscore_and_extension() {
    for url in ["helper", "_helper", "helper.scss", "_helper.scss"] {
        let css = build(&format!("@use \"pkg:dep-a/{url}\" as x;"), "partial").unwrap();
        assert!(css.contains(".helper"), "{url}: {css}");
        assert!(css.contains(".inner"), "{url}: {css}");
    }
}

#[test]
fn relative_imports_stay_in_the_package() {
    let css = build("@use \"pkg:dep-a/helper\";", "relative").unwrap();
    assert!(css.contains(".inner"), "{css}");
}

#[test]
fn same_file_by_two_routes_loads_once() {
    let css = build(
        "@use \"pkg:dep-a/sub/inner\"; @use \"pkg:dep-a/helper\";",
        "once",
    )
    .unwrap();
    assert_eq!(css.matches(".inner").count(), 1, "{css}");
}

#[test]
fn unknown_crate() {
    let error = build("@use \"pkg:nope\" as x;", "unknown")
        .unwrap_err()
        .to_string();
    assert!(error.contains("no package named nope"), "{error}");
    assert!(error.contains("dep-a"), "{error}");
}

#[test]
fn crate_that_did_not_opt_in() {
    let error = build("@use \"pkg:plain\" as x;", "plain")
        .unwrap_err()
        .to_string();
    assert!(error.contains("no package named plain"), "{error}");
}

#[test]
fn missing_partial() {
    let error = build("@use \"pkg:dep-a/missing\";", "missing")
        .unwrap_err()
        .to_string();
    assert!(error.contains("pkg:dep-a/missing"), "{error}");
    assert!(error.contains("no such stylesheet"), "{error}");
}

#[test]
fn package_paths_do_not_escape() {
    let error = build("@use \"pkg:dep-a/../../plain/style/index\";", "escape")
        .unwrap_err()
        .to_string();
    assert!(error.contains("stays inside the package"), "{error}");
}

#[test]
fn load_path_resolves_plain_urls() {
    let mut request = request("@use \"index\";", "loadpath2");
    request.load_paths = vec![fixture("plain").join("style")];
    let css = trug::build_css(&request).unwrap();
    assert!(css.contains(".plain"), "{css}");
}

#[test]
fn relative_to_the_entry() {
    let mut request = request("@use \"sibling\";", "sibling");
    std::fs::write(
        request.entry.with_file_name("_sibling.scss"),
        ".sib { a: b }",
    )
    .unwrap();
    request.query = query("app");
    let css = trug::build_css(&request).unwrap();
    assert!(css.contains(".sib"), "{css}");
}

#[test]
fn features_module_values() {
    let css = build(
        "@use \"pkg:dep-a/features\" as f;\n\
         .x::after { content: \"#{f.$name} #{f.$version}\"; }\n\
         .y::after { content: \"#{map-keys(f.$features)}\"; }",
        "values",
    )
    .unwrap();
    assert!(css.contains("dep-a 1.0.0"), "{css}");
    assert!(css.contains("base"), "{css}");
}

#[test]
fn empty_features_map_works() {
    let css = build(
        "@use \"sass:map\"; @use \"pkg:custom-b/features\" as f;\n\
         @if map.has-key(f.$features, \"x\") { .no { a: b } } @else { .yes { a: b } }\n\
         @if f.enabled(\"x\") { .no2 { a: b } }",
        "empty",
    )
    .unwrap();
    assert!(css.contains(".yes"), "{css}");
    assert!(!css.contains(".no"), "{css}");
}

#[test]
fn stylesheet_prunes_by_feature() {
    let off = build("@use \"pkg:dep-a\" as x;", "off").unwrap();
    assert!(off.contains(".a-base"), "{off}");
    assert!(!off.contains(".a-extra"), "{off}");

    let mut on = request("@use \"pkg:dep-a\" as x;", "on");
    on.query.features = vec!["extra".to_owned()];
    let on = trug::build_css(&on).unwrap();
    assert!(on.contains(".a-base"), "{on}");
    assert!(on.contains(".a-extra {"), "{on}");
    assert!(on.contains(".a-extra-partial"), "{on}");
}

#[test]
fn app_fixture_stylesheet() {
    let mut request = CssRequest::new(fixture("app").join("style/app.scss"));
    request.query = query("app");
    let css = trug::build_css(&request).unwrap();
    assert!(
        css.contains(".app") && css.contains(".a-base") && css.contains(".b-main"),
        "{css}"
    );
}

fn build_extra(source: &str, tag: &str) -> String {
    let mut request = request(source, tag);
    request.query.features = vec!["extra".to_owned()];
    trug::build_css(&request).unwrap()
}

#[test]
fn gated_pkg_load() {
    let off = build("@use \"pkg:dep-a/gated\";", "gate-pkg-off").unwrap();
    assert!(!off.contains(".a-gated"), "{off}");

    let on = build_extra("@use \"pkg:dep-a/gated\";", "gate-pkg-on");
    assert!(on.contains(".a-gated"), "{on}");
}

#[test]
fn gated_relative_load() {
    let off = build("@use \"pkg:dep-a/uses_gated\";", "gate-rel-off").unwrap();
    assert!(off.contains(".uses-gated"), "{off}");
    assert!(!off.contains(".a-gated"), "{off}");

    let on = build_extra("@use \"pkg:dep-a/uses_gated\";", "gate-rel-on");
    assert!(on.contains(".uses-gated"), "{on}");
    assert!(on.contains(".a-gated"), "{on}");
}

#[test]
fn gate_with_unknown_feature() {
    let mut request = request("@use \"pkg:bad-gate\" as x;", "gate-unknown");
    request.query = query("bad-gate");
    let error = trug::build_css(&request).unwrap_err().to_string();
    assert!(
        error.contains("gate.index: no feature named nope"),
        "{error}"
    );
}

#[test]
fn source_map_is_embedded_on_request() {
    let mut req = request("@use \"pkg:dep-a\" as x;", "map");
    assert!(!trug::build_css(&req).unwrap().contains("sourceMappingURL"));
    req.source_map = true;
    let css = trug::build_css(&req).unwrap();
    assert!(css.contains(".a-base"), "{css}");
    assert!(
        css.contains("/*# sourceMappingURL=data:application/json;charset=utf-8;base64,"),
        "{css}"
    );
}

const SILENT_CHILD: &str = "TRUG_TEST_SILENT_CHILD";

#[test]
fn library_is_silent_without_a_reporter() {
    if std::env::var_os(SILENT_CHILD).is_some() {
        build(
            "@warn \"careful\";\n@debug \"look\";\na { b: c }",
            "silent-child",
        )
        .unwrap();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "library_is_silent_without_a_reporter",
            "--nocapture",
        ])
        .env(SILENT_CHILD, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("careful") && !stderr.contains("look"),
        "{stderr}"
    );
}

#[test]
fn warnings_go_to_the_reporter() {
    use std::sync::{Arc, Mutex};
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut request = request("@warn \"careful\";\n@debug \"look\";", "reporter");
    let seen = events.clone();
    request.reporter = trug::Reporter::new(move |event| {
        let text = match event {
            trug::Event::Warning(text) => format!("warning {text}"),
            trug::Event::Debug(text) => format!("debug {text}"),
            other => format!("{other:?}"),
        };
        seen.lock().unwrap().push(text);
    });
    trug::build_css(&request).unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[0].starts_with("warning ") && events[0].contains("careful"));
    assert!(events[1].starts_with("debug ") && events[1].contains("look"));
}
