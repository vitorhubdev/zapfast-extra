use std::fs::{self, File};
use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Release, install};

const DOWNLOAD_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const REPOSITORY: &str = "vitorhubdev/Vespera";

fn release_api(version: &str) -> String {
    format!("https://api.github.com/repos/{REPOSITORY}/releases/tags/v{version}")
}

fn download_path(version: &str, name: &str) -> String {
    format!("/{REPOSITORY}/releases/download/v{version}/{name}")
}

/// A GitHub release listing or asset for this repository, and no other.
fn belongs_to_repository(url: &reqwest::Url, version: &str, name: Option<&str>) -> bool {
    if !Source::GitHub.allowed(url) {
        return false;
    }
    match (url.host_str(), name) {
        (Some("api.github.com"), None) => {
            url.path() == format!("/repos/{REPOSITORY}/releases/tags/v{version}")
                || url.path() == format!("/repos/{REPOSITORY}/releases/latest")
                || url.path() == format!("/repos/{REPOSITORY}/releases")
        }
        (Some("github.com"), Some(name)) => url.path() == download_path(version, name),
        _ => false,
    }
}

#[derive(Clone, Debug, Default)]
pub enum Source {
    #[default]
    GitHub,
    #[cfg(feature = "demo")]
    Local(String),
}

impl Source {
    pub fn latest(&self) -> String {
        match self {
            Self::GitHub => super::LATEST_RELEASE_URL.into(),
            #[cfg(feature = "demo")]
            Self::Local(base) => format!("{base}/latest.json"),
        }
    }

    fn release(&self, version: &str) -> String {
        match self {
            Self::GitHub => release_api(version),
            #[cfg(feature = "demo")]
            Self::Local(base) => format!("{base}/latest.json"),
        }
    }

    #[cfg(feature = "demo")]
    pub fn local(value: &str) -> Result<Self> {
        let url = reqwest::Url::parse(value)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "The demo update feed must use a loopback HTTP address"
        );
        Ok(Self::Local(value.trim_end_matches('/').into()))
    }

    fn allowed(&self, url: &reqwest::Url) -> bool {
        match self {
            Self::GitHub => {
                url.scheme() == "https"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && matches!(
                        url.host_str(),
                        Some(
                            "api.github.com"
                                | "github.com"
                                | "release-assets.githubusercontent.com"
                                | "objects.githubusercontent.com"
                        )
                    )
            }
            #[cfg(feature = "demo")]
            Self::Local(base) => {
                reqwest::Url::parse(base).is_ok_and(|base| url.origin() == base.origin())
            }
        }
    }
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

fn asset<'a>(metadata: &'a Metadata, name: &str) -> Result<&'a Asset> {
    let matches: Vec<_> = metadata
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "The release has no unique {name} download"
    );
    let asset = matches[0];
    ensure!(
        asset.size > 0 && asset.size <= DOWNLOAD_LIMIT,
        "Invalid update download size"
    );
    Ok(asset)
}

fn checksum(text: &str, name: &str) -> Result<String> {
    let mut found = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(digest), Some(file), None) = (fields.next(), fields.next(), fields.next())
            && file.trim_start_matches('*') == name
        {
            ensure!(found.is_none(), "Duplicate checksum for the update");
            ensure!(
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Invalid update checksum"
            );
            found = Some(digest.to_ascii_lowercase());
        }
    }
    found.context("The release is missing the update checksum")
}

pub fn download(
    release: &Release,
    source: &Source,
    channel: super::Channel,
    progress: impl Fn(u64, u64),
) -> Result<install::Prepared> {
    let installation = install::detect()?;
    download_for(release, source, channel, installation, progress)
}

pub fn download_for(
    release: &Release,
    source: &Source,
    channel: super::Channel,
    installation: install::Installation,
    progress: impl Fn(u64, u64),
) -> Result<install::Prepared> {
    ensure!(
        super::installable(channel, &release.version),
        "Invalid release version"
    );
    let policy = source.clone();
    let http = reqwest::blocking::Client::builder()
        .user_agent(format!("Vespera/{}", super::vespera_version()))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(15 * 60))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() < 5 && policy.allowed(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Update redirect is not allowed")
            }
        }))
        .build()?;
    let metadata: Metadata = serde_json::from_reader(
        http.get(source.release(&release.version))
            .send()?
            .error_for_status()?
            .take(1024 * 1024),
    )?;
    ensure!(
        !metadata.draft
            && (!metadata.prerelease || channel == super::Channel::Testing)
            && metadata.tag_name == format!("v{}", release.version),
        "The release changed. Check for updates again."
    );
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "aarch64" | "x86_64") => "macos-universal",
        _ => bail!("Use the download page for this operating system or architecture"),
    };
    let stem = format!("vespera-v{}-{target}", release.version);
    let name = match installation.kind {
        #[cfg(target_os = "macos")]
        install::Kind::MacBundle => format!("{stem}.dmg"),
        install::Kind::WindowsInstaller => format!("{stem}-setup.exe"),
        install::Kind::Portable if cfg!(windows) => format!("{stem}.zip"),
        install::Kind::Portable => format!("{stem}.tar.gz"),
    };
    let package = asset(&metadata, &name)?;
    let checksums = asset(&metadata, "checksums.txt")?;
    for candidate in [package, checksums] {
        let url = reqwest::Url::parse(&candidate.browser_download_url)?;
        ensure!(
            source.allowed(&url),
            "Update download is not on the release host"
        );
        if matches!(source, Source::GitHub) {
            ensure!(
                belongs_to_repository(&url, &release.version, Some(&candidate.name)),
                "Update asset does not belong to this release"
            );
        }
    }
    let mut checksum_text = String::new();
    http.get(&checksums.browser_download_url)
        .send()?
        .error_for_status()?
        .take(1024 * 1024)
        .read_to_string(&mut checksum_text)?;
    let expected = checksum(&checksum_text, &name)?;
    let directory = install::staging(&installation)?;
    let result = (|| -> Result<install::Prepared> {
        let archive = directory.join(&name);
        let mut output = File::create(&archive)?;
        let mut response = http
            .get(&package.browser_download_url)
            .send()?
            .error_for_status()?;
        let mut hash = Sha256::new();
        let mut received = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = response.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            received += count as u64;
            ensure!(
                received <= package.size,
                "Update download exceeds its published size"
            );
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
            if received % (1024 * 1024) < count as u64 || received == package.size {
                progress(received, package.size);
            }
        }
        output.sync_all()?;
        drop(output);
        ensure!(
            received == package.size,
            "The update download was interrupted"
        );
        ensure!(
            super::hex(&hash.finalize()) == expected,
            "The download couldn't be verified. Try downloading it again."
        );
        #[cfg(target_os = "macos")]
        if installation.kind == install::Kind::MacBundle {
            super::macos::validate_download(&archive, &installation, &release.version)?;
            return Ok(install::Prepared {
                sha256: expected.clone(),
                installation,
                directory: directory.clone(),
                payload: archive,
                version: release.version.clone(),
            });
        }
        let payload = if installation.kind == install::Kind::WindowsInstaller {
            archive.clone()
        } else {
            let executable = if cfg!(windows) {
                "vespera.exe"
            } else {
                "vespera"
            };
            let payload = directory.join(executable);
            install::extract(&archive, &format!("{stem}/{executable}"), &payload)?;
            install::verify_version(&payload, &release.version)?;
            fs::remove_file(&archive)?;
            payload
        };
        Ok(install::Prepared {
            sha256: install::hash(&payload)?,
            installation,
            directory: directory.clone(),
            payload,
            version: release.version.clone(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&directory);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "demo", any(target_os = "windows", target_os = "linux")))]
    #[test]
    fn update_downloads_preserve_integrity_checks() {
        use std::net::TcpListener;
        for interrupted in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let platform = if cfg!(windows) {
                "pc-windows-msvc.zip"
            } else {
                "unknown-linux-gnu.tar.gz"
            };
            let name = format!("vespera-v0.8.0-{}-{platform}", std::env::consts::ARCH);
            let payload = b"damaged download";
            let hash = if interrupted {
                crate::updates::hex(&Sha256::digest(payload))
            } else {
                "0".repeat(64)
            };
            let checksums = format!("{hash}  {name}\n");
            let metadata = serde_json::json!({"tag_name":"v0.8.0", "assets":[
                {"name":name,"size":payload.len() + usize::from(interrupted),"browser_download_url":format!("{base}/package")},
                {"name":"checksums.txt","size":checksums.len(),"browser_download_url":format!("{base}/checksums")}
            ]}).to_string();
            let expected_urls =
                ["latest.json", "checksums", "package"].map(|path| format!("GET /{path} HTTP/1.1"));
            let server = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                for body in [
                    metadata.into_bytes(),
                    checksums.into_bytes(),
                    payload.to_vec(),
                ]
                .into_iter()
                .zip(expected_urls)
                {
                    let (body, expected_request) = body;
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                assert!(
                                    std::time::Instant::now() < deadline,
                                    "update did not reach its configured route"
                                );
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(error) => panic!("fixture listener: {error}"),
                        }
                    };
                    // Accepted sockets can inherit the nonblocking listener's
                    // mode. Read the request with the bounded timeout below.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = [0; 4096];
                    let size = stream.read(&mut request).unwrap();
                    assert!(
                        String::from_utf8_lossy(&request[..size]).starts_with(&expected_request)
                    );
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(&body).unwrap();
                }
            });
            let directory = std::env::temp_dir()
                .join(format!("vespera-download-test-{}", rand::random::<u64>()));
            fs::create_dir(&directory).unwrap();
            let target = directory.join("vespera");
            fs::write(&target, b"original").unwrap();
            let installation = install::Installation {
                executable: target.clone(),
                kind: install::Kind::Portable,
            };
            let release = Release {
                version: "0.8.0".into(),
                url: base.clone(),
            };
            let error = download_for(
                &release,
                &Source::local(&base).unwrap(),
                crate::updates::Channel::Stable,
                installation,
                |_, _| {},
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(if interrupted {
                    "interrupted"
                } else {
                    "verified"
                }),
                "{error:#}"
            );
            assert_eq!(fs::read(&target).unwrap(), b"original");
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
            server.join().unwrap();
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[cfg(all(feature = "demo", any(target_os = "windows", target_os = "linux")))]
    #[test]
    fn testing_channel_installs_candidates_stable_still_refuses() {
        use std::net::TcpListener;
        // A release candidate behind Testing must sail past the version and
        // metadata gates and only fail on integrity, like a stable build;
        // the same candidate behind Stable must be refused at the gate.
        let version = "0.8.0-rc.1";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let platform = if cfg!(windows) {
            "pc-windows-msvc.zip"
        } else {
            "unknown-linux-gnu.tar.gz"
        };
        let name = format!("vespera-v{version}-{}-{platform}", std::env::consts::ARCH);
        let payload = b"damaged download";
        let checksums = format!("{}  {name}\n", "0".repeat(64));
        let metadata = serde_json::json!({"tag_name":format!("v{version}"),"prerelease":true,"assets":[
            {"name":name,"size":payload.len(),"browser_download_url":format!("{base}/package")},
            {"name":"checksums.txt","size":checksums.len(),"browser_download_url":format!("{base}/checksums")}
        ]})
        .to_string();
        let expected_urls =
            ["latest.json", "checksums", "package"].map(|path| format!("GET /{path} HTTP/1.1"));
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for body in [
                metadata.into_bytes(),
                checksums.into_bytes(),
                payload.to_vec(),
            ]
            .into_iter()
            .zip(expected_urls)
            {
                let (body, expected_request) = body;
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "update did not reach its route"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture listener: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let size = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..size]).starts_with(&expected_request));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let directory =
            std::env::temp_dir().join(format!("vespera-rc-test-{}", rand::random::<u64>()));
        fs::create_dir(&directory).unwrap();
        let target = directory.join("vespera");
        fs::write(&target, b"original").unwrap();
        let installation = install::Installation {
            executable: target.clone(),
            kind: install::Kind::Portable,
        };
        let release = Release {
            version: version.into(),
            url: base.clone(),
        };
        // Stable refuses the candidate at the version gate, before any download.
        let stable = download_for(
            &release,
            &Source::local(&base).unwrap(),
            crate::updates::Channel::Stable,
            install::Installation {
                executable: target.clone(),
                kind: install::Kind::Portable,
            },
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            stable.to_string().contains("Invalid release version"),
            "stable refuses candidates: {stable:#}"
        );
        let error = download_for(
            &release,
            &Source::local(&base).unwrap(),
            crate::updates::Channel::Testing,
            installation,
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("verified"),
            "candidate reaches integrity: {error:#}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"original");
        server.join().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }
    // rc download test end
    #[test]
    fn checksums_must_be_unique_valid_and_for_the_exact_asset() {
        let digest = "a".repeat(64);
        let valid = format!("{digest}  app.zip\n");
        assert_eq!(checksum(&valid, "app.zip").unwrap(), digest);
        assert!(checksum(&valid, "other.zip").is_err());
        assert!(checksum(&(valid.clone() + &valid), "app.zip").is_err());
        assert!(checksum("invalid app.zip", "app.zip").is_err());
        let marked = format!("# commit abcdef\n{digest} *app.zip\n");
        assert_eq!(checksum(&marked, "app.zip").unwrap(), digest);
    }

    #[test]
    fn release_and_download_urls_belong_only_to_this_repository() {
        let version = "1.0.106";
        let name = "vespera-v1.0.106-x86_64-pc-windows-msvc.zip";
        let release = reqwest::Url::parse(&Source::GitHub.release(version)).unwrap();
        let latest = reqwest::Url::parse(&Source::GitHub.latest()).unwrap();
        let download = reqwest::Url::parse(&format!(
            "https://github.com/{REPOSITORY}/releases/download/v{version}/{name}"
        ))
        .unwrap();
        assert!(belongs_to_repository(&release, version, None));
        assert!(belongs_to_repository(&latest, version, None));
        assert!(belongs_to_repository(&download, version, Some(name)));
        let previous = format!("{}-extra", format!("{}{}", "zap", "fast"));
        let previous_download = reqwest::Url::parse(&format!(
            "https://github.com/vitorhubdev/{previous}/releases/download/v{version}/{name}"
        ))
        .unwrap();
        let previous_release = reqwest::Url::parse(&format!(
            "https://api.github.com/repos/vitorhubdev/{previous}/releases/tags/v{version}"
        ))
        .unwrap();
        let other = reqwest::Url::parse(&format!(
            "https://github.com/other/Vespera/releases/download/v{version}/{name}"
        ))
        .unwrap();
        assert!(!belongs_to_repository(
            &previous_download,
            version,
            Some(name)
        ));
        assert!(!belongs_to_repository(&previous_release, version, None));
        assert!(!belongs_to_repository(&other, version, Some(name)));
        assert!(!Source::GitHub.allowed(&reqwest::Url::parse("https://example.com/file").unwrap()));
    }

    #[test]
    fn redirects_cannot_leave_release_hosts() {
        for address in [
            "http://github.com/file",
            "https://github.com.attacker.invalid/file",
            "https://example.com/file",
        ] {
            assert!(!Source::GitHub.allowed(&reqwest::Url::parse(address).unwrap()));
        }
        assert!(Source::GitHub.allowed(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
        ));
    }
}
