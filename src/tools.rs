//! Build tools trug finds on `PATH` or downloads into its cache: wasm-opt and wasm-bindgen.

use std::env::consts::{ARCH, EXE_SUFFIX, OS};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::build::cache_dir;

/// The binaryen release wasm-opt is downloaded from.
pub const BINARYEN_VERSION: &str = "version_133";

fn binaryen_platform(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        ("macos", "x86_64") => "x86_64-macos",
        ("macos", "aarch64") => "arm64-macos",
        ("windows", "x86_64") => "x86_64-windows",
        ("windows", "aarch64") => "arm64-windows",
        _ => return None,
    })
}

fn binaryen_url(version: &str, platform: &str) -> String {
    format!(
        "https://github.com/WebAssembly/binaryen/releases/download/{version}/binaryen-{version}-{platform}.tar.gz"
    )
}

fn bindgen_target(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

fn bindgen_url(version: &str, target: &str) -> String {
    format!(
        "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/{version}/wasm-bindgen-{version}-{target}.tar.gz"
    )
}

/// `wasm-opt` on `PATH`, else the cached binaryen one, else downloaded; `None` after a warning
/// when none can be had.
pub fn wasm_opt() -> Option<PathBuf> {
    let on_path = PathBuf::from("wasm-opt");
    if runs(&on_path) {
        return Some(on_path);
    }
    let root = cache_dir().join("binaryen").join(BINARYEN_VERSION);
    let binary = format!("bin/wasm-opt{EXE_SUFFIX}");
    match cached_or_install(&root, &binary, install_wasm_opt) {
        Ok(path) => Some(path),
        Err(error) => {
            eprintln!("trug: {error}; release wasm is not optimized");
            None
        }
    }
}

fn install_wasm_opt(root: &Path) -> Result<(), String> {
    let platform = binaryen_platform(OS, ARCH)
        .ok_or_else(|| format!("no prebuilt wasm-opt for {OS}-{ARCH}"))?;
    eprintln!("trug: downloading wasm-opt {BINARYEN_VERSION}");
    let url = binaryen_url(BINARYEN_VERSION, platform);
    install(root, &url, &format!("{url}.sha256"), true, |rel| {
        let name = rel.file_name()?.to_str()?;
        let wanted = if rel.starts_with("bin") {
            name == format!("wasm-opt{EXE_SUFFIX}") || name.ends_with(".dll")
        } else {
            rel.starts_with("lib") && (name.ends_with(".dylib") || name.contains(".so"))
        };
        wanted.then(|| rel.to_owned())
    })
}

/// The path of `binary` under `root`, running `install` first when it is missing. Installs
/// rename into place, so an existing file is complete.
fn cached_or_install(
    root: &Path,
    binary: &str,
    install: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let path = root.join(binary);
    if !path.is_file() {
        install(root)?;
        if !path.is_file() {
            return Err(format!("{} was not installed", path.display()));
        }
    }
    Ok(path)
}

/// wasm-bindgen at exactly `version`: on `PATH`, else cached, else downloaded, else
/// `cargo install`ed into the cache.
pub fn wasm_bindgen(version: &str) -> Result<PathBuf, String> {
    let on_path = PathBuf::from("wasm-bindgen");
    if has_version(&on_path, version) {
        return Ok(on_path);
    }
    let root = cache_dir().join("wasm-bindgen").join(version);
    let cached = root.join(format!("bin/wasm-bindgen{EXE_SUFFIX}"));
    if has_version(&cached, version) {
        return Ok(cached);
    }
    match download_bindgen(&root, version) {
        Ok(()) if has_version(&cached, version) => return Ok(cached),
        Ok(()) => eprintln!("trug: downloaded wasm-bindgen is not version {version}"),
        Err(error) => eprintln!("trug: {error}"),
    }
    cargo_install_bindgen(&root, version)?;
    if !has_version(&cached, version) {
        return Err(format!("could not install wasm-bindgen-cli {version}"));
    }
    Ok(cached)
}

fn download_bindgen(root: &Path, version: &str) -> Result<(), String> {
    let target = bindgen_target(OS, ARCH)
        .ok_or_else(|| format!("no prebuilt wasm-bindgen for {OS}-{ARCH}"))?;
    eprintln!("trug: downloading wasm-bindgen {version}");
    let url = bindgen_url(version, target);
    let binary = format!("wasm-bindgen{EXE_SUFFIX}");
    install(root, &url, &format!("{url}.sha256sum"), false, |rel| {
        (rel == Path::new(&binary)).then(|| Path::new("bin").join(&binary))
    })
}

fn cargo_install_bindgen(root: &Path, version: &str) -> Result<(), String> {
    eprintln!(
        "trug: installing wasm-bindgen-cli {version} into {}",
        root.display()
    );
    let status = Command::new("cargo")
        .args([
            "install",
            "wasm-bindgen-cli",
            "--locked",
            "--force",
            "--version",
            version,
            "--root",
        ])
        .arg(root)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cargo install: {e}"))?;
    if !status.success() {
        return Err(format!("could not install wasm-bindgen-cli {version}"));
    }
    Ok(())
}

fn runs(binary: &Path) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn has_version(binary: &Path, version: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout).trim() == format!("wasm-bindgen {version}")
        })
}

/// Downloads the tar.gz at `url`, checks it against the sha256 file at `checksum` (when one is
/// published, or always when `required`), and extracts the files `pick` maps to a path into
/// `root`, which replaces any previous `root` in one rename.
fn install(
    root: &Path,
    url: &str,
    checksum: &str,
    required: bool,
    pick: impl Fn(&Path) -> Option<PathBuf>,
) -> Result<(), String> {
    let parent = root.parent().expect("cache roots have a parent");
    let name = root.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.tmp{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let result = (|| {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("trug/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .timeout(None)
            .build()
            .map_err(|e| e.to_string())?;
        let archive = tmp.join("archive.tar.gz");
        let digest = download(&client, url, &archive)?;
        match get(&client, checksum)? {
            Some(response) => {
                let text = response.text().map_err(|e| format!("{checksum}: {e}"))?;
                if !checksum_matches(&text, &digest) {
                    return Err(format!("{url}: sha256 does not match {checksum}"));
                }
            }
            None if required => return Err(format!("{checksum}: not found")),
            None => {}
        }
        let out = tmp.join("out");
        if extract(&archive, &out, &pick)? == 0 {
            return Err(format!("{url}: nothing to extract"));
        }
        let _ = fs::remove_dir_all(root);
        if let Err(error) = fs::rename(&out, root)
            && !root.is_dir()
        {
            return Err(format!("{}: {error}", root.display()));
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// `None` on 404.
fn get(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<Option<reqwest::blocking::Response>, String> {
    let response = client.get(url).send().map_err(|e| format!("{url}: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response
        .error_for_status()
        .map(Some)
        .map_err(|e| format!("{url}: {e}"))
}

/// Streams `url` into `to`; returns its sha256.
fn download(client: &reqwest::blocking::Client, url: &str, to: &Path) -> Result<String, String> {
    let mut response = get(client, url)?.ok_or_else(|| format!("{url}: not found"))?;
    let mut file = File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = response.read(&mut buf).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("{}: {e}", to.display()))?;
    }
    Ok(hex(&hasher.finalize()))
}

/// Whether a `<hex>  <name>` checksum file names `digest`.
fn checksum_matches(text: &str, digest: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|sum| sum.eq_ignore_ascii_case(digest))
}

/// Extracts each regular file `pick` maps (by its path below the archive's top directory) to a
/// path under `out`; returns how many it extracted.
fn extract(
    archive: &Path,
    out: &Path,
    pick: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<usize, String> {
    let error = |e: io::Error| format!("{}: {e}", archive.display());
    let file = File::open(archive).map_err(error)?;
    let mut tar = tar::Archive::new(GzDecoder::new(io::BufReader::new(file)));
    let mut count = 0;
    for entry in tar.entries().map_err(error)? {
        let mut entry = entry.map_err(error)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(error)?.into_owned();
        let rel: PathBuf = path.components().skip(1).collect();
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            continue;
        }
        let Some(to) = pick(&rel) else {
            continue;
        };
        let to = out.join(to);
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        entry.unpack(&to).map_err(error)?;
        count += 1;
    }
    Ok(count)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trug-tools-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn asset_names_follow_the_host() {
        assert_eq!(binaryen_platform("linux", "x86_64"), Some("x86_64-linux"));
        assert_eq!(binaryen_platform("macos", "aarch64"), Some("arm64-macos"));
        assert_eq!(
            binaryen_platform("windows", "x86_64"),
            Some("x86_64-windows")
        );
        assert_eq!(binaryen_platform("freebsd", "x86_64"), None);
        assert_eq!(
            binaryen_url("version_133", "x86_64-linux"),
            "https://github.com/WebAssembly/binaryen/releases/download/version_133/binaryen-version_133-x86_64-linux.tar.gz"
        );
        assert_eq!(
            bindgen_target("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            bindgen_target("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(bindgen_target("linux", "riscv64"), None);
        assert_eq!(
            bindgen_url("0.2.130", "x86_64-apple-darwin"),
            "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.130/wasm-bindgen-0.2.130-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn cached_tool_is_used_without_installing() {
        let root = temp("cached");
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("bin/wasm-opt"), "").unwrap();
        let path = cached_or_install(&root, "bin/wasm-opt", |_| panic!("installed")).unwrap();
        assert_eq!(path, root.join("bin/wasm-opt"));
        let missing = root.join("missing");
        let err = cached_or_install(&missing, "bin/wasm-opt", |_| Ok(())).unwrap_err();
        assert!(err.contains("was not installed"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn checksums_compare_the_first_field() {
        assert!(checksum_matches("ABcd  file.tar.gz\n", "abcd"));
        assert!(checksum_matches("abcd\n", "abcd"));
        assert!(!checksum_matches("abce  file.tar.gz", "abcd"));
        assert!(!checksum_matches("", "abcd"));
    }

    #[test]
    fn extracts_only_picked_files() {
        let dir = temp("extract");
        let archive = dir.join("a.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        ));
        for (path, data) in [
            ("top/bin/wasm-opt", &b"opt"[..]),
            ("top/bin/wasm-dis", b"dis"),
            ("top/lib/libbinaryen.a", b"a"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        let out = dir.join("out");
        let count = extract(&archive, &out, &|rel| {
            (rel == Path::new("bin/wasm-opt")).then(|| rel.to_owned())
        })
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(fs::read(out.join("bin/wasm-opt")).unwrap(), b"opt");
        assert!(!out.join("bin/wasm-dis").exists() && !out.join("lib").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
