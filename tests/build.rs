mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::fixture;

fn work_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/trug-tests")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn build(app: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_trug"))
        .current_dir(app)
        .env("CARGO_TARGET_DIR", work_dir().join("target"))
        .env("TRUG_CACHE_DIR", work_dir().join("cache"))
        .args(["build", "--release", "-v"])
        .output()
        .unwrap()
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// Installs wasm-bindgen-cli into target/trug-tests/cache on the first run, which takes minutes.
#[test]
#[ignore]
fn full_app_dist_layout() {
    let root = work_dir().join("full");
    let _ = std::fs::remove_dir_all(&root);
    copy_dir(&fixture("dep-a"), &root.join("dep-a"));
    let app = root.join("app");
    copy_dir(&fixture("full-app"), &app);

    let output = build(&app);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(app.join("hook-ran").exists());

    let dist = app.join("dist");
    let names = listing(&dist);
    let find = |prefix: &str, suffix: &str| -> String {
        let found: Vec<_> = names
            .iter()
            .filter(|n| n.starts_with(prefix) && n.ends_with(suffix))
            .collect();
        assert_eq!(found.len(), 1, "{prefix}*{suffix} in {names:?}");
        found[0].clone()
    };
    let icon = find("icon-", ".png");
    let css = find("main-", ".css");
    let plain = find("plain-", ".css");
    let js = find("full_app", ".js");
    let wasm = find("full_app", ".wasm");
    assert!(names.contains(&"assets".to_owned()));
    assert!(names.contains(&"static".to_owned()));
    assert!(!names.contains(&"data".to_owned()));
    assert!(names.contains(&"snippets".to_owned()));
    assert_eq!(names.len(), 9, "{names:?}");
    let snippets = dist.join("snippets");
    let snippet_dir = listing(&snippets).remove(0);
    assert!(snippets.join(snippet_dir).join("inline0.js").exists());

    assert_eq!(
        std::fs::read_to_string(dist.join("assets/hello.txt")).unwrap(),
        "hello from assets\n"
    );
    assert!(dist.join("static/files/nested/data.json").exists());

    let sheet = std::fs::read_to_string(dist.join(&css)).unwrap();
    assert!(sheet.contains(".a-extra-partial"), "{sheet}");
    assert!(sheet.contains("font-size: 20px"), "{sheet}");
    assert!(sheet.contains("border-left"), "{sheet}");
    assert!(
        std::fs::read_to_string(dist.join(&plain))
            .unwrap()
            .contains(".plain")
    );

    let index = std::fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(!index.contains("data-trug"), "{index}");
    assert!(!index.contains("scss") && !index.contains("static/files"));
    assert!(index.contains("<base href=\"/app/\">"), "{index}");
    for name in [&icon, &css, &plain, &js, &wasm] {
        assert!(index.contains(&format!("/app/{name}")), "{name} in {index}");
    }
}

#[test]
#[ignore]
fn build_swap_and_failures() {
    let app = work_dir().join("app");
    let _ = std::fs::remove_dir_all(&app);
    copy_dir(&fixture("wasm-app"), &app);
    let dist = app.join("dist");

    let output = build(&app);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = listing(&dist);
    assert_eq!(first.len(), 3, "{first:?}");
    let index = std::fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(!index.contains("_trug/reload"));
    assert!(index.starts_with("<!DOCTYPE html>\r\n<html>\r\n<head>\r\n  <meta charset=\"utf-8\">\r\n  <title>Fixture</title>\r\n  "));
    assert!(index.ends_with("\r\n</head>\r\n<body>\r\n</body>\r\n</html>\r\n"));
    assert!(!index.contains("data-trug"));
    for name in first.iter().filter(|n| *n != "index.html") {
        assert!(index.contains(name.as_str()), "{index}");
    }

    std::fs::write(app.join("fail-hook"), "").unwrap();
    let output = build(&app);
    assert!(!output.status.success());
    assert_eq!(listing(&dist), first);
    std::fs::remove_file(app.join("fail-hook")).unwrap();

    let lib = app.join("src/lib.rs");
    let good = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, "this is not rust").unwrap();
    let output = build(&app);
    assert!(!output.status.success());
    assert_eq!(listing(&dist), first);
    assert_eq!(
        listing(&app)
            .iter()
            .filter(|n| n.starts_with(".dist"))
            .count(),
        0
    );

    std::fs::write(&lib, good.replace("42", "43")).unwrap();
    let output = build(&app);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let second = listing(&dist);
    assert_eq!(second.len(), 3);
    assert_ne!(second, first);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Generating bindings"));
}
