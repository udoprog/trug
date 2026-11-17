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
    assert!(String::from_utf8_lossy(&output.stderr).contains("wasm-bindgen:"));
}
