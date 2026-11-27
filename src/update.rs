//! Update check against the latest GitHub release, verified download, silent install.

use crate::http;
use std::io::Write;
use std::path::{Path, PathBuf};
use windows::Win32::Security::Cryptography::{
    BCRYPT_HASH_HANDLE, BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash, BCryptDestroyHash,
    BCryptFinishHash, BCryptHashData,
};

const DOWNLOAD_PREFIX: &str = "https://github.com/Kurohiko-id/Swara/releases/download/";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub digest: String,
    pub size: u64,
}

/// Bytes of the GitHub API reply we are willing to parse.
const API_BYTE_CAP: u64 = 1 << 20;
const SETUP_BYTE_CAP: u64 = 64 << 20;
const REDIRECT_HOPS: usize = 5;
/// GitHub answers a release download with a redirect to one of its storage hosts.
const DOWNLOAD_HOSTS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// The GitHub reply is untrusted: malformed assets are skipped and `pick_asset` vets the rest.
fn parse_release(body: &[u8]) -> Result<Release, String> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("release json: {e}"))?;
    let tag = json["tag_name"]
        .as_str()
        .filter(|t| !t.is_empty() && t.len() <= 64)
        .ok_or("bad tag_name")?;
    let assets = json["assets"]
        .as_array()
        .ok_or("missing assets")?
        .iter()
        .filter_map(|item| {
            Some(Asset {
                name: item["name"].as_str()?.into(),
                url: item["browser_download_url"].as_str()?.into(),
                digest: item["digest"].as_str()?.into(),
                size: item["size"].as_u64()?,
            })
        })
        .collect();
    Ok(Release {
        tag: tag.into(),
        assets,
    })
}

/// `[v]MAJOR.MINOR.PATCH[-PRE]`; anything else is None.
fn version(v: &str) -> Option<([u64; 3], Option<&str>)> {
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    if pre.is_some_and(|p| {
        p.is_empty()
            || !p
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    }) {
        return None;
    }
    let mut parts = core.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let p = parts
            .next()
            .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?;
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some((out, pre))
}

pub fn newer_than(candidate: &str, running: &str) -> bool {
    let (Some((cand, cand_pre)), Some((run, run_pre))) = (version(candidate), version(running))
    else {
        return false;
    };
    if cand != run {
        return cand > run;
    }
    match (cand_pre, run_pre) {
        (None, Some(_)) => true,
        // Lexical, so rc10 sorts below rc9; acceptable with one pre-release per version.
        (Some(c), Some(r)) => c > r,
        _ => false,
    }
}

fn digest_hex(digest: &str) -> Option<&str> {
    digest
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The installer of this release: exact name, exact download URL in our repo, a SHA-256
/// digest, and within the size cap. None when any check fails.
pub fn pick_asset(r: &Release) -> Option<&Asset> {
    // The tag ends up in a file name and a URL path, so only a well-formed version passes.
    version(&r.tag)?;
    let ver = r.tag.strip_prefix('v').unwrap_or(&r.tag);
    let name = format!("Swara-Setup-{ver}.exe");
    let url = format!("{DOWNLOAD_PREFIX}{}/{name}", r.tag);
    r.assets.iter().find(|a| {
        a.name == name
            && a.url == url
            && digest_hex(&a.digest).is_some()
            && a.size <= SETUP_BYTE_CAP
    })
}

/// `actual_hex` against a GitHub `sha256:<hex>` digest.
pub fn verify_digest(actual_hex: &str, digest: &str) -> Result<(), String> {
    let expected = digest_hex(digest).ok_or("bad digest")?;
    if actual_hex.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err("installer sha-256 differs from release digest".into())
    }
}

struct Sha256(BCRYPT_HASH_HANDLE);

impl Sha256 {
    fn new() -> Result<Self, String> {
        let mut h = BCRYPT_HASH_HANDLE::default();
        // SAFETY: the SHA-256 pseudo-handle needs no open/close; `h` receives a new hash
        // object that `Drop` destroys.
        unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, None, None, 0) }
            .ok()
            .map_err(|e| e.to_string())?;
        Ok(Self(h))
    }

    fn update(&mut self, data: &[u8]) -> Result<(), String> {
        // SAFETY: `self.0` is a live hash object; `data` is a valid slice.
        unsafe { BCryptHashData(self.0, data, 0) }
            .ok()
            .map_err(|e| e.to_string())
    }

    fn finish(self) -> Result<String, String> {
        let mut out = [0u8; 32];
        // SAFETY: `self.0` is a live hash object; SHA-256 output is exactly 32 bytes.
        unsafe { BCryptFinishHash(self.0, &mut out, 0) }
            .ok()
            .map_err(|e| e.to_string())?;
        Ok(out.iter().fold(String::with_capacity(64), |mut hex, byte| {
            hex.push_str(&format!("{byte:02x}"));
            hex
        }))
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        // SAFETY: created in `new`, destroyed once.
        unsafe {
            let _ = BCryptDestroyHash(self.0);
        }
    }
}

pub fn latest_release() -> Result<Release, String> {
    let mut body = Vec::new();
    let reply = http::get(
        "api.github.com",
        "/repos/Kurohiko-id/Swara/releases/latest",
        "Accept: application/vnd.github+json",
        10000,
        API_BYTE_CAP,
        &mut |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        },
    )?;
    if reply.status != 200 {
        return Err(format!("HTTP {}", reply.status));
    }
    parse_release(&body)
}

/// `https://<allowed host>/<path>` split into host and path.
fn download_target(url: &str) -> Result<(&str, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("download url not https")?;
    let (host, _) = rest.split_once('/').ok_or("download url without path")?;
    if !DOWNLOAD_HOSTS.contains(&host) {
        return Err(format!("download host not allowed: {host}"));
    }
    Ok((host, &rest[host.len()..]))
}

/// Downloads to `%TEMP%\<asset name>` and checks the SHA-256. On any failure, the checksum
/// included, the file is deleted. `report` gets the bytes received so far.
pub fn download_installer(
    asset: &Asset,
    report: &mut dyn FnMut(u64),
) -> Result<VerifiedInstaller, String> {
    // Vet again: the caller may hand in an asset that did not come from `pick_asset`.
    let tag = asset
        .url
        .strip_prefix(DOWNLOAD_PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .ok_or("asset url outside the release path")?
        .0;
    let check = Release {
        tag: tag.into(),
        assets: vec![asset.clone()],
    };
    if pick_asset(&check).is_none() {
        return Err("asset failed validation".into());
    }
    let path = std::env::temp_dir().join(&asset.name);
    let result = download_to(asset, &path, report);
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result.map(|()| VerifiedInstaller(path))
}

fn download_to(asset: &Asset, path: &Path, report: &mut dyn FnMut(u64)) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new()?;
    let mut received = 0u64;
    let mut url = asset.url.clone();
    // The API size makes a wrong download fail early; the digest is still what decides.
    let cap = match asset.size {
        1..=SETUP_BYTE_CAP => asset.size,
        _ => SETUP_BYTE_CAP,
    };
    for _ in 0..=REDIRECT_HOPS {
        let (host, p) = download_target(&url)?;
        let reply = http::get(
            host,
            p,
            "Accept: application/octet-stream",
            30000,
            cap,
            &mut |chunk| {
                file.write_all(chunk).map_err(|e| e.to_string())?;
                hash.update(chunk)?;
                received += chunk.len() as u64;
                report(received);
                Ok(())
            },
        )?;
        match reply.status {
            200 => {
                file.sync_all().map_err(|e| e.to_string())?;
                return verify_digest(&hash.finish()?, &asset.digest);
            }
            301 | 302 | 303 | 307 | 308 => {
                url = reply.location.ok_or("redirect without location")?;
            }
            s => return Err(format!("HTTP {s}")),
        }
    }
    Err("too many redirects".into())
}

/// An installer whose SHA-256 matched its release digest. Only `download_installer` makes one,
/// so `run_silent` cannot be handed an unchecked file.
#[derive(Debug)]
pub struct VerifiedInstaller(PathBuf);

impl VerifiedInstaller {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A file name `pick_asset` could have produced, e.g. `Swara-Setup-0.2.0.exe`.
fn is_installer_name(name: &str) -> bool {
    name.strip_prefix("Swara-Setup-")
        .and_then(|v| v.strip_suffix(".exe"))
        .is_some_and(|v| version(v).is_some())
}

/// Deletes installers left in %TEMP% by an earlier update. The running setup still holds
/// its file during the update, so this runs at the next start instead.
pub fn remove_stale_installers() {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for e in dir.flatten() {
        // A symlink planted under such a name is removed itself, never its target.
        if e.file_name().to_str().is_some_and(is_installer_name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Starts the installer and returns without waiting for it.
pub fn run_silent(installer: VerifiedInstaller) -> Result<(), String> {
    std::process::Command::new(&installer.0)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/UPDATE"])
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-2 SHA-256 test vector for the message "abc".
    const SHA_OF_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn sha_digest() -> String {
        format!("sha256:{SHA_OF_ABC}")
    }

    fn entry(name: &str, url: &str, digest: &str) -> Asset {
        Asset {
            name: name.into(),
            url: url.into(),
            digest: digest.into(),
            size: 1,
        }
    }

    fn rel(tag: &str, assets: Vec<Asset>) -> Release {
        Release {
            tag: tag.into(),
            assets,
        }
    }

    fn setup_for(tag: &str, ver: &str) -> Asset {
        let file = format!("Swara-Setup-{ver}.exe");
        let url = format!("{DOWNLOAD_PREFIX}{tag}/{file}");
        entry(&file, &url, &sha_digest())
    }

    #[test]
    fn version_ordering() {
        let newer = [
            ("v0.3.12", "0.3.4"),
            ("0.4.0", "v0.3.77"),
            ("v2.0.0", "2.0.0-beta2"),
            ("1.0.0-rc2", "1.0.0-rc1"),
        ];
        for (cand, run) in newer {
            assert!(newer_than(cand, run), "{cand} > {run}");
        }
        let not_newer = [
            ("v2.0.0-beta2", "2.0.0"),
            ("v0.3.4", "0.3.4"),
            ("v0.3.4", "0.3.12"),
            ("canary", "0.3.4"),
            ("v0.y.1", "0.3.4"),
            ("0.3.4", "dev-build"),
            ("", ""),
        ];
        for (cand, run) in not_newer {
            assert!(!newer_than(cand, run), "{cand} <= {run}");
        }
    }

    #[test]
    fn only_the_matching_setup_is_chosen() {
        let portable = entry(
            "Swara-0.3.0-portable.zip",
            &format!("{DOWNLOAD_PREFIX}v0.3.0/Swara-0.3.0-portable.zip"),
            &sha_digest(),
        );
        let stale = setup_for("v0.3.0", "0.2.5");
        let set = rel(
            "v0.3.0",
            vec![portable, stale, setup_for("v0.3.0", "0.3.0")],
        );
        assert_eq!(
            pick_asset(&set).map(|a| a.name.as_str()),
            Some("Swara-Setup-0.3.0.exe")
        );

        let mut short_hash = setup_for("v0.3.0", "0.3.0");
        short_hash.digest = "sha256:beef".into();
        assert_eq!(pick_asset(&rel("v0.3.0", vec![short_hash])), None);
        let mut other_algo = setup_for("v0.3.0", "0.3.0");
        other_algo.digest = format!("sha1:{SHA_OF_ABC}");
        assert_eq!(pick_asset(&rel("v0.3.0", vec![other_algo])), None);
        let mut oversized = setup_for("v0.3.0", "0.3.0");
        oversized.size = SETUP_BYTE_CAP + 1;
        assert_eq!(pick_asset(&rel("v0.3.0", vec![oversized])), None);
        // A crafted tag must not let the saved file land outside %TEMP%.
        let escape = rel("v..\\..\\y", vec![setup_for("v..\\..\\y", "..\\..\\y")]);
        assert_eq!(pick_asset(&escape), None);
        let climb = rel("v../../y", vec![setup_for("v../../y", "../../y")]);
        assert_eq!(pick_asset(&climb), None);
        assert_eq!(pick_asset(&rel("v0.3.0", vec![])), None);
    }

    #[test]
    fn urls_outside_own_releases_are_refused() {
        let file = "Swara-Setup-0.3.0.exe";
        let urls = [
            format!("https://downloads.invalid/{file}"),
            format!("http://github.com/Kurohiko-id/Swara/releases/download/v0.3.0/{file}"),
            format!("https://github.com/Kurohiko-id/Fork/releases/download/v0.3.0/{file}"),
            format!(
                "https://github.com.mirror.invalid/Kurohiko-id/Swara/releases/download/v0.3.0/{file}"
            ),
            format!("{DOWNLOAD_PREFIX}v0.3.0/../../../../a/b/releases/download/v0.3.0/{file}"),
        ];
        for url in urls {
            let set = rel("v0.3.0", vec![entry(file, &url, &sha_digest())]);
            assert_eq!(pick_asset(&set), None, "{url}");
        }
    }

    #[test]
    fn hash_compare_is_strict_but_case_blind() {
        let mut hasher = Sha256::new().unwrap();
        hasher.update(b"ab").unwrap();
        hasher.update(b"c").unwrap();
        let actual = hasher.finish().unwrap();
        assert_eq!(actual, SHA_OF_ABC);
        assert!(verify_digest(&actual, &sha_digest()).is_ok());
        let upper = format!("sha256:{}", SHA_OF_ABC.to_ascii_uppercase());
        assert!(verify_digest(&actual, &upper).is_ok());
        let zeros = format!("sha256:{}", "f".repeat(64));
        assert!(verify_digest(&actual, &zeros).is_err());
        assert!(verify_digest(&actual, SHA_OF_ABC).is_err());
        assert!(verify_digest(&actual, "").is_err());
        assert!(verify_digest("", "sha256:").is_err());
    }

    #[test]
    fn malformed_release_json_is_an_error() {
        let junk: [&[u8]; 6] = [
            b"",
            b"<html>",
            b"[1, 2]",
            b"{\"assets\": []}",
            b"{\"tag_name\": true}",
            b"{\"documentation_url\": \"x\"}",
        ];
        for body in junk {
            let shown = String::from_utf8_lossy(body);
            assert!(parse_release(body).is_err(), "{shown}");
        }
        let huge_tag = format!("{{\"tag_name\": \"{}\", \"assets\": []}}", "9".repeat(65));
        assert!(parse_release(huge_tag.as_bytes()).is_err());

        let body = format!(
            r#"{{"tag_name": "v0.3.0", "assets": [
                {{"name": "Swara-Setup-0.3.0.exe", "size": "big"}},
                {{"name": "Swara-Setup-0.3.0.exe", "size": 4096,
                  "browser_download_url": "{DOWNLOAD_PREFIX}v0.3.0/Swara-Setup-0.3.0.exe",
                  "digest": "sha256:{SHA_OF_ABC}"}}
            ]}}"#
        );
        let parsed = parse_release(body.as_bytes()).unwrap();
        assert_eq!(parsed.tag, "v0.3.0");
        assert_eq!(parsed.assets.len(), 1);
        assert_eq!(parsed.assets[0].size, 4096);
        assert!(pick_asset(&parsed).is_some());
    }

    #[test]
    #[ignore]
    fn queries_github_for_real() {
        let answer = latest_release();
        println!("latest release lookup: {answer:?}");
    }

    #[test]
    fn leftover_setup_names() {
        for keep in ["Swara-Setup-0.3.0.exe", "Swara-Setup-2.1.0-beta.3.exe"] {
            assert!(is_installer_name(keep), "{keep}");
        }
        for skip in [
            "Swara-Setup-.exe",
            "Swara-Setup-v.exe",
            "Swara-Setup-0.3.0.exe.bak",
            "SWARA-SETUP-0.3.0.EXE",
            "Another-Setup-0.3.0.exe",
            "Swara-Setup-..\\0.3.0.exe",
        ] {
            assert!(!is_installer_name(skip), "{skip}");
        }
    }
}
