use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::thread;
use teams_cli::update::{self, UpdateOptions};
use tiny_http::{Header, Response, Server};

fn build_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (name, data) in entries {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_str = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let cksum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum_str = format!("{cksum:06o}\0 ");
        header[148..156].copy_from_slice(cksum_str.as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            tar.extend(std::iter::repeat_n(0u8, 512 - rem));
        }
    }
    tar.extend(std::iter::repeat_n(0u8, 1024));

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn fake_macho(tag: &str) -> Vec<u8> {
    let mut bytes = vec![0xCF, 0xFA, 0xED, 0xFE];
    bytes.extend_from_slice(tag.as_bytes());
    bytes
}

#[test]
fn semver_and_tag_helpers() {
    assert_eq!(update::parse_semver("v0.3.0"), Some((0, 3, 0)));
    assert_eq!(update::parse_semver("1.2.3-rc.1"), Some((1, 2, 3)));
    assert_eq!(update::parse_semver("invalid"), None);
    assert!(update::is_newer("0.3.0", "0.3.1"));
    assert!(update::is_newer("0.3.0", "v0.4.0"));
    assert!(!update::is_newer("0.3.0", "0.3.0"));
    assert!(!update::is_newer("0.3.1", "0.3.0"));

    assert_eq!(update::normalize_tag("0.3.0").unwrap(), "v0.3.0");
    assert_eq!(update::normalize_tag("v0.3.0").unwrap(), "v0.3.0");
    assert!(update::normalize_tag("").is_err());
    assert!(update::normalize_tag("../evil").is_err());
}

#[test]
fn trusted_release_urls_and_checksums() {
    assert!(update::is_trusted_release_url(
        "https://api.github.com/repos/sasanktumpati/teams-cli/releases/latest",
        false
    ));
    assert!(update::is_trusted_release_url(
        "https://objects.githubusercontent.com/github-production-release-asset/foo",
        false
    ));
    assert!(!update::is_trusted_release_url("http://github.com/a/b", false));
    assert!(!update::is_trusted_release_url(
        "https://evil.example/teams.tar.gz",
        false
    ));
    assert!(update::is_trusted_release_url("http://127.0.0.1:9999/test", true));

    let digest = hex::encode(Sha256::digest(b"hello"));
    let sums = format!(
        "{digest}  teams-v0.4.0-aarch64-apple-darwin.tar.gz\n\
         0000000000000000000000000000000000000000000000000000000000000000 *other.tar.gz\n"
    );
    assert_eq!(
        update::parse_sha256_for_asset(&sums, "teams-v0.4.0-aarch64-apple-darwin.tar.gz").as_deref(),
        Some(digest.as_str())
    );
    assert!(update::verify_sha256(b"hello", &digest).is_ok());
    assert!(update::verify_sha256(b"tampered", &digest).is_err());
}

#[test]
fn tar_gz_extraction_handles_dot_slash_and_rejects_traversal() {
    let payload = fake_macho("v0.4.0");
    let archive = build_tar_gz(&[("README.md", b"# teams-cli\n"), ("./teams", &payload)]);
    let extracted = update::extract_binary_from_tar_gz(&archive, "teams").unwrap();
    assert_eq!(extracted, payload);
    assert!(update::is_valid_macho(&extracted));

    let bad = build_tar_gz(&[("../teams", &payload)]);
    assert!(update::extract_binary_from_tar_gz(&bad, "teams").is_err());
}

#[test]
fn mock_github_release_check_install_and_checksum_failure() {
    let new_bin = fake_macho("v0.4.0-new");
    let archive = Arc::new(build_tar_gz(&[("README.md", b"hi"), ("teams", &new_bin)]));
    let good_hash = hex::encode(Sha256::digest(&*archive));
    let asset_name = "teams-v0.4.0-aarch64-apple-darwin.tar.gz";

    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let addr = server.server_addr().to_ip().unwrap();
    let base = format!("http://{addr}");

    let archive_srv = archive.clone();
    let base_srv = base.clone();
    let good_hash_srv = good_hash.clone();
    let srv = server.clone();
    let handle = thread::spawn(move || {
        while let Ok(req) = srv.recv() {
            let url = req.url().to_string();
            if url == "/repos/owner/repo/releases/latest" {
                let body = json!({
                    "tag_name": "v0.4.0",
                    "assets": [
                        {"name": asset_name, "browser_download_url": format!("{base_srv}/redirect/{asset_name}")},
                        {"name": "SHA256SUMS.txt", "browser_download_url": format!("{base_srv}/good-sums")}
                    ]
                })
                .to_string();
                let _ = req.respond(Response::from_string(body));
            } else if url == "/repos/owner/repo/releases/tags/v0.4.0-bad" {
                let body = json!({
                    "tag_name": "v0.4.0",
                    "assets": [
                        {"name": asset_name, "browser_download_url": format!("{base_srv}/download/{asset_name}")},
                        {"name": "SHA256SUMS.txt", "browser_download_url": format!("{base_srv}/bad-sums")}
                    ]
                })
                .to_string();
                let _ = req.respond(Response::from_string(body));
            } else if url == format!("/redirect/{asset_name}") {
                let loc = Header::from_bytes("Location", format!("{base_srv}/download/{asset_name}")).unwrap();
                let _ = req.respond(Response::empty(302).with_header(loc));
            } else if url == format!("/download/{asset_name}") {
                let _ = req.respond(Response::from_data((*archive_srv).clone()));
            } else if url == "/good-sums" {
                let _ = req.respond(Response::from_string(format!("{good_hash_srv}  {asset_name}\n")));
            } else if url == "/bad-sums" {
                let _ = req.respond(Response::from_string(format!("{:064x}  {asset_name}\n", 0u8)));
            } else {
                let _ = req.respond(Response::empty(404));
            }
        }
    });

    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("teams");
    fs::write(&dest, b"old-binary").unwrap();

    let check = update::run_update_with(
        &UpdateOptions {
            check_only: true,
            version: None,
            force: false,
        },
        &base,
        "owner/repo",
        "0.3.0",
        "aarch64-apple-darwin",
        &dest,
        true,
        true,
    )
    .unwrap();
    assert!(check.update_available);
    assert!(!check.updated);
    assert_eq!(fs::read(&dest).unwrap(), b"old-binary");

    let up_to_date = update::run_update_with(
        &UpdateOptions {
            check_only: false,
            version: None,
            force: false,
        },
        &base,
        "owner/repo",
        "0.4.0",
        "aarch64-apple-darwin",
        &dest,
        true,
        true,
    )
    .unwrap();
    assert!(!up_to_date.update_available);
    assert!(!up_to_date.updated);
    assert_eq!(fs::read(&dest).unwrap(), b"old-binary");

    let bad_err = update::run_update_with(
        &UpdateOptions {
            check_only: false,
            version: Some("v0.4.0-bad".to_string()),
            force: true,
        },
        &base,
        "owner/repo",
        "0.3.0",
        "aarch64-apple-darwin",
        &dest,
        true,
        true,
    )
    .unwrap_err();
    assert!(bad_err.contains("SHA-256 mismatch"), "{bad_err}");
    assert_eq!(fs::read(&dest).unwrap(), b"old-binary");

    let installed = update::run_update_with(
        &UpdateOptions {
            check_only: false,
            version: None,
            force: false,
        },
        &base,
        "owner/repo",
        "0.3.0",
        "aarch64-apple-darwin",
        &dest,
        true,
        true,
    )
    .unwrap();
    assert!(installed.updated);
    assert_eq!(installed.sha256.as_deref(), Some(good_hash.as_str()));
    assert_eq!(fs::read(&dest).unwrap(), new_bin);
    assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);

    server.unblock();
    handle.join().unwrap();
}
