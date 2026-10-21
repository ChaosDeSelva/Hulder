use anyhow::{Context as _, Result, ensure};
use semver::Version;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

pub const VERSION: &str = "0.1.3";
const PREFIX: &str = "https://github.com/ChaosDeSelva/Hulder/releases/download/";
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub digest: Option<String>,
    pub size: u64,
}
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    assets: Vec<Asset>,
}
#[derive(Clone, Debug)]
pub struct Update {
    pub version: Version,
    pub asset: Asset,
}

pub fn select(json: &str, installed: &str) -> Result<Option<Update>> {
    let installed = Version::parse(installed)?;
    let releases: Vec<Release> = serde_json::from_str(json)?;
    let mut candidates = Vec::new();
    for release in releases.into_iter().filter(|release| !release.draft) {
        let Ok(version) = Version::parse(release.tag_name.trim_start_matches('v')) else {
            continue;
        };
        if version <= installed {
            continue;
        }
        let name = format!("Hulder-{version}-macos-arm64.dmg");
        let url = format!("{PREFIX}v{version}/{name}");
        if let Some(asset) = release
            .assets
            .into_iter()
            .find(|asset| asset.name == name && asset.browser_download_url == url)
        {
            validate_asset(&asset)?;
            candidates.push(Update { version, asset });
        }
    }
    Ok(candidates
        .into_iter()
        .max_by(|left, right| left.version.cmp(&right.version)))
}
fn validate_asset(asset: &Asset) -> Result<&str> {
    ensure!(
        asset.size > 0 && asset.size <= 512 * 1024 * 1024,
        "Installer size is invalid"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .context("Release has no SHA256 digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid SHA256 digest"
    );
    Ok(digest)
}
fn checked(output: Output, operation: &str) -> Result<Output> {
    ensure!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(800)
            .collect::<String>()
    );
    Ok(output)
}
pub fn fetch(installed: &str) -> Result<Option<Update>> {
    let output = checked(
        Command::new("/usr/bin/curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--max-time",
                "25",
                "--max-filesize",
                "2097152",
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "User-Agent: Hulder-Updates",
                "https://api.github.com/repos/ChaosDeSelva/Hulder/releases?per_page=100",
            ])
            .output()?,
        "GitHub update check",
    )?;
    select(std::str::from_utf8(&output.stdout)?, installed)
}
pub fn download(update: &Update, root: &Path) -> Result<PathBuf> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "Installer updates currently support Apple silicon Macs"
    );
    let expected_url = format!(
        "{PREFIX}v{}/Hulder-{}-macos-arm64.dmg",
        update.version, update.version
    );
    ensure!(
        update.asset.browser_download_url == expected_url,
        "Unexpected installer URL"
    );
    let expected_digest = validate_asset(&update.asset)?;
    fs::create_dir_all(root)?;
    let directory = root.join(format!(
        "download-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let destination = directory.join(format!("Hulder-{}-macos-arm64.dmg", update.version));
    let result = (|| -> Result<()> {
        checked(
            Command::new("/usr/bin/curl")
                .args([
                    "--fail",
                    "--silent",
                    "--show-error",
                    "--location",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--max-time",
                    "600",
                    "--max-filesize",
                    "536870912",
                    "--output",
                ])
                .arg(&destination)
                .arg(&expected_url)
                .output()?,
            "Installer download",
        )?;
        ensure!(
            fs::metadata(&destination)?.len() == update.asset.size,
            "Installer size does not match release"
        );
        let checksum = checked(
            Command::new("/usr/bin/shasum")
                .args(["-a", "256"])
                .arg(&destination)
                .output()?,
            "Checksum verification",
        )?;
        let checksum = String::from_utf8(checksum.stdout)?;
        ensure!(
            checksum
                .split_whitespace()
                .next()
                .is_some_and(|digest| digest.eq_ignore_ascii_case(expected_digest)),
            "Installer checksum mismatch"
        );
        checked(
            Command::new("/usr/bin/codesign")
                .args([
                    "--verify",
                    "--strict",
                    "-R",
                    "=anchor apple generic and certificate leaf[subject.OU] = \"3LS9LD88JX\"",
                ])
                .arg(&destination)
                .output()?,
            "Publisher signature verification",
        )?;
        checked(
            Command::new("/usr/sbin/spctl")
                .args([
                    "--assess",
                    "--type",
                    "open",
                    "--context",
                    "context:primary-signature",
                ])
                .arg(&destination)
                .output()?,
            "Apple notarization verification",
        )?;
        Ok(())
    })();
    if result.is_err() {
        fs::remove_dir_all(&directory).context("Cleaning failed update download")?;
    }
    result?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn release(version: &str) -> serde_json::Value {
        let name = format!("Hulder-{version}-macos-arm64.dmg");
        serde_json::json!({"tag_name":format!("v{version}"),"draft":false,"prerelease":true,"assets":[{"name":name,"browser_download_url":format!("{PREFIX}v{version}/{name}"),"digest":format!("sha256:{}", "a".repeat(64)),"size":100}]})
    }
    #[test]
    fn selects_newest_compatible_release() {
        let input = serde_json::json!([release("0.1.9"), release("0.1.10"), release("0.1.2")]);
        assert_eq!(
            select(&input.to_string(), "0.1.3")
                .expect("parse")
                .expect("update")
                .version
                .to_string(),
            "0.1.10"
        );
    }
    #[test]
    fn ignores_current_older_and_drafts() {
        let mut draft = release("9.0.0");
        draft["draft"] = true.into();
        assert!(
            select(
                &serde_json::json!([release("0.1.3"), release("0.1.2"), draft]).to_string(),
                "0.1.3"
            )
            .expect("parse")
            .is_none()
        );
    }
    #[test]
    fn refuses_untrusted_urls_and_missing_digest() {
        let mut bad = release("0.1.4");
        bad["assets"][0]["browser_download_url"] = "https://example.com/installer.dmg".into();
        assert!(
            select(&serde_json::json!([bad]).to_string(), "0.1.3")
                .expect("parse")
                .is_none()
        );
        let mut bad = release("0.1.4");
        bad["assets"][0]["digest"] = serde_json::Value::Null;
        assert!(select(&serde_json::json!([bad]).to_string(), "0.1.3").is_err());
    }
    #[test]
    fn rejects_malformed_and_oversized_assets() {
        assert!(select("not json", "0.1.3").is_err());
        let mut bad = release("0.1.4");
        bad["assets"][0]["size"] = u64::MAX.into();
        assert!(select(&serde_json::json!([bad]).to_string(), "0.1.3").is_err());
    }
}
