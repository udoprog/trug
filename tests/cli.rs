mod common;

use std::process::Command;

use common::fixture;

fn trug() -> Command {
    Command::new(env!("CARGO_BIN_EXE_trug"))
}

fn cargo_args(command: &mut Command) -> &mut Command {
    command
        .arg("--manifest-path")
        .arg(fixture("app").join("Cargo.toml"))
        .arg("--offline")
}

#[test]
fn css_to_stdout() {
    let output = cargo_args(trug().arg("css").arg(fixture("app").join("style/app.scss")))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let css = String::from_utf8(output.stdout).unwrap();
    assert!(
        css.contains(".a-base") && !css.contains(".a-extra"),
        "{css}"
    );
}

#[test]
fn css_to_file_with_features() {
    let out = std::env::temp_dir().join(format!("trug-cli-{}/out/app.css", std::process::id()));
    let output = cargo_args(trug().arg("css").arg(fixture("app").join("style/app.scss")))
        .args(["-F", "extra", "--style", "compressed", "-o"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let css = std::fs::read_to_string(&out).unwrap();
    assert!(css.contains(".a-extra{"), "{css}");
}

#[test]
fn failed_build_keeps_the_old_file() {
    let dir = std::env::temp_dir().join(format!("trug-cli-keep-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("bad.scss");
    std::fs::write(&entry, "@use \"pkg:nope\" as x;").unwrap();
    let out = dir.join("out.css");
    std::fs::write(&out, "old").unwrap();
    let output = cargo_args(trug().arg("css").arg(&entry))
        .arg("-o")
        .arg(&out)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no package named nope"));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "old");
}

#[test]
fn packages_lists_features() {
    let output = cargo_args(trug().arg("packages")).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("dep-a 1.0.0"), "{text}");
    assert!(text.contains("[base]"), "{text}");
    assert!(text.contains("custom-b 0.3.0"), "{text}");
}
