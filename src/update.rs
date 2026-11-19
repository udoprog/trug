//! `trug update`: replaces the running binary with the latest GitHub release.

use std::env::consts::{ARCH, EXE_SUFFIX, OS};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::tools::{client, download, extract, get};

const LATEST: &str = "https://api.github.com/repos/udoprog/trug/releases/latest";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Updates the running trug to the latest release, unless it is already up to date.
pub fn update() -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let client = client()?;
    let text = get(&client, LATEST)?
        .ok_or("no release found")?
        .text()
        .map_err(|e| format!("{LATEST}: {e}"))?;
    let release: Release = serde_json::from_str(&text).map_err(|e| format!("{LATEST}: {e}"))?;
    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer(latest, current)? {
        println!("trug {current} is up to date");
        return Ok(());
    }
    let asset = select_asset(&release.assets, ARCH, OS)?;
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("current executable: {e}"))?;
    let dir = exe.parent().ok_or("current executable has no directory")?;
    let work = dir.join(format!(".trug-update{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    eprintln!("trug: downloading {}", asset.name);
    let result = (|| {
        let archive = work.join(&asset.name);
        download(&client, &asset.browser_download_url, &archive)?;
        let new = work.join(format!("trug{EXE_SUFFIX}"));
        extract_binary(&archive, &new)?;
        replace(&new, &exe)
    })();
    let _ = fs::remove_dir_all(&work);
    result?;
    println!("trug updated from {current} to {latest}");
    Ok(())
}

/// Whether the dotted numeric version `latest` is newer than `current`.
fn is_newer(latest: &str, current: &str) -> Result<bool, String> {
    Ok(parse_version(latest)? > parse_version(current)?)
}

fn parse_version(version: &str) -> Result<Vec<u64>, String> {
    let mut parts = version
        .split('.')
        .map(|p| {
            p.parse::<u64>()
                .map_err(|_| format!("bad version `{version}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    Ok(parts)
}

/// The asset named `trug-<version>-<arch>-<os>.<ext>` for this platform.
fn select_asset<'a>(assets: &'a [Asset], arch: &str, os: &str) -> Result<&'a Asset, String> {
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let suffix = format!("-{arch}-{os}.{ext}");
    assets
        .iter()
        .find(|a| a.name.starts_with("trug-") && a.name.ends_with(&suffix))
        .ok_or_else(|| format!("no prebuilt trug for {arch}-{os}"))
}

fn extract_binary(archive: &Path, to: &Path) -> Result<(), String> {
    let name = format!("trug{EXE_SUFFIX}");
    if archive.extension().is_some_and(|e| e == "zip") {
        let file = File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        let mut zip = zip::ZipArchive::new(io::BufReader::new(file))
            .map_err(|e| format!("{}: {e}", archive.display()))?;
        let mut entry = zip
            .by_name(&name)
            .map_err(|e| format!("{}: {name}: {e}", archive.display()))?;
        let mut out = File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("{}: {e}", to.display()))?;
        return Ok(());
    }
    let dir = to.parent().unwrap_or(Path::new("."));
    let out = dir.join("extracted");
    let found = extract(archive, 0, &out, &|rel| {
        (rel == Path::new(&name)).then(|| rel.to_owned())
    })?;
    if found == 0 {
        return Err(format!("{}: no {name} in the archive", archive.display()));
    }
    fs::rename(out.join(&name), to).map_err(|e| format!("{}: {e}", to.display()))
}

/// Renames `new` over `exe`; on windows the running exe is first moved aside to `trug.old.exe`.
fn replace(new: &Path, exe: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(new, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{}: {e}", new.display()))?;
    }
    let error = |e: io::Error| format!("{}: {e}", exe.display());
    if cfg!(windows) {
        let old: PathBuf = exe.with_file_name("trug.old.exe");
        let _ = fs::remove_file(&old);
        fs::rename(exe, &old).map_err(error)?;
        if let Err(e) = fs::rename(new, exe) {
            let _ = fs::rename(&old, exe);
            return Err(error(e));
        }
        return Ok(());
    }
    fs::rename(new, exe).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|n| Asset {
                name: n.to_string(),
                browser_download_url: format!("https://example.com/{n}"),
            })
            .collect()
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.1.1", "0.1.0").unwrap());
        assert!(is_newer("0.10.0", "0.9.9").unwrap());
        assert!(is_newer("1.0", "0.9.9").unwrap());
        assert!(!is_newer("0.1.0", "0.1.0").unwrap());
        assert!(!is_newer("0.1", "0.1.0").unwrap());
        assert!(!is_newer("0.0.9", "0.1.0").unwrap());
        assert!(is_newer("x", "0.1.0").is_err());
    }

    #[test]
    fn selects_the_platform_asset() {
        let list = assets(&[
            "trug-0.2.0-x86_64-linux.tar.gz",
            "trug-0.2.0-aarch64-macos.tar.gz",
            "trug-0.2.0-x86_64-windows.zip",
            "checksums.txt",
        ]);
        let name = |arch, os| select_asset(&list, arch, os).map(|a| a.name.as_str());
        assert_eq!(
            name("x86_64", "linux"),
            Ok("trug-0.2.0-x86_64-linux.tar.gz")
        );
        assert_eq!(
            name("aarch64", "macos"),
            Ok("trug-0.2.0-aarch64-macos.tar.gz")
        );
        assert_eq!(
            name("x86_64", "windows"),
            Ok("trug-0.2.0-x86_64-windows.zip")
        );
        assert_eq!(
            name("aarch64", "linux"),
            Err("no prebuilt trug for aarch64-linux".to_string())
        );
        assert!(name("x86_64", "macos").is_err());
    }
}
