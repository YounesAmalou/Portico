//! The release scripts, which the Linux and macOS release jobs run (Windows builds its zips
//! in its own job): Unix-only.
#![cfg(unix)]

use std::{fs, process::Command};

use tempfile::TempDir;

fn script(name: &str) -> String {
    format!("{}/scripts/{name}", env!("CARGO_MANIFEST_DIR"))
}

const CHANGELOG: &str = "# Changelog

## [Unreleased]

## [0.2.0] - 2026-11-01

### Added

- second release

## [0.1.0] - 2026-10-13

### Added

- first release
- second line
";

fn changelog_notes(version: &str) -> std::process::Output {
    let dir = TempDir::new().unwrap();
    let changelog = dir.path().join("CHANGELOG.md");
    fs::write(&changelog, CHANGELOG).unwrap();
    Command::new(script("changelog-notes.sh"))
        .arg(version)
        .arg(&changelog)
        .output()
        .unwrap()
}

#[test]
fn changelog_notes_extracts_only_the_requested_section() {
    let output = changelog_notes("0.1.0");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "### Added\n\n- first release\n- second line\n"
    );
}

#[test]
fn changelog_notes_fails_for_a_missing_version() {
    let output = changelog_notes("0.9.0");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing changelog section for 0.9.0")
    );
}

const TARGET: &str = "x86_64-unknown-linux-musl";

#[test]
fn package_release_builds_a_tarball_named_after_the_target_with_license_files() {
    let dir = TempDir::new().unwrap();
    let binary = dir.path().join("portico");
    fs::write(&binary, "#!/bin/sh\necho portico\n").unwrap();
    let out = dir.path().join("out");
    let output = Command::new(script("package-release.sh"))
        .args(["0.1.0", TARGET])
        .arg(&binary)
        .arg(&out)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tarball = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        tarball.file_name().unwrap().to_str().unwrap(),
        format!("portico-v0.1.0-{TARGET}.tar.gz")
    );
    // SHA256SUMS, written by the release workflow, is the only checksum file.
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);

    let unpacked = dir.path().join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&unpacked)
        .status()
        .unwrap();
    assert!(status.success());
    let root = unpacked.join(format!("portico-v0.1.0-{TARGET}"));
    for file in ["portico", "LICENSE-MIT", "LICENSE-APACHE", "README.md"] {
        assert!(root.join(file).is_file(), "{file} missing from the tarball");
    }
    use std::os::unix::fs::PermissionsExt;
    assert_ne!(
        fs::metadata(root.join("portico"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}

#[test]
fn package_release_requires_the_target() {
    let output = Command::new(script("package-release.sh"))
        .args(["0.1.0", "portico", "out"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("<target>"));
}

#[test]
fn homebrew_formula_names_both_macos_tarballs_with_their_checksums() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path()
            .join("portico-v1.2.3-aarch64-apple-darwin.tar.gz"),
        "arm",
    )
    .unwrap();
    fs::write(
        dir.path().join("portico-v1.2.3-x86_64-apple-darwin.tar.gz"),
        "intel",
    )
    .unwrap();
    let output = Command::new(script("homebrew-formula.sh"))
        .arg("1.2.3")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formula = String::from_utf8(output.stdout).unwrap();
    assert!(
        formula.starts_with("class Portico < Formula\n"),
        "{formula}"
    );
    assert!(!formula.contains('@'), "a placeholder is left: {formula}");
    for expected in [
        "version \"1.2.3\"",
        "/releases/download/v1.2.3/portico-v1.2.3-aarch64-apple-darwin.tar.gz\"",
        "/releases/download/v1.2.3/portico-v1.2.3-x86_64-apple-darwin.tar.gz\"",
        // SHA-256 of "arm" and of "intel".
        "sha256 \"ddf7ff5ebd9d66ce161466c1c0262430fa04de32b0e420ee3f489e2e2112e386\"",
        "sha256 \"96eebba49dbbf422d245f02290f9d4ed0eb02da9daa6bbceefb162800ff42481\"",
    ] {
        assert!(
            formula.contains(expected),
            "the formula lacks {expected}:\n{formula}"
        );
    }
}

#[test]
fn homebrew_formula_requires_both_tarballs() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path()
            .join("portico-v1.2.3-aarch64-apple-darwin.tar.gz"),
        "arm",
    )
    .unwrap();
    let output = Command::new(script("homebrew-formula.sh"))
        .arg("1.2.3")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("x86_64-apple-darwin.tar.gz"));
}

#[test]
fn package_release_zips_a_windows_build_with_portico_exe_at_the_root() {
    let target = "x86_64-pc-windows-msvc";
    let dir = TempDir::new().unwrap();
    let binary = dir.path().join("portico.exe");
    fs::write(&binary, "MZ").unwrap();
    let out = dir.path().join("out");
    let output = Command::new(script("package-release.sh"))
        .args(["0.1.0", target])
        .arg(&binary)
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let zip = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        zip.file_name().unwrap().to_str().unwrap(),
        format!("portico-v0.1.0-{target}.zip")
    );
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);

    // winget's manifest names portico.exe at the root of the zip.
    let listing = Command::new("bsdtar")
        .arg("-tf")
        .arg(&zip)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let mut entries: Vec<String> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        ["LICENSE-APACHE", "LICENSE-MIT", "README.md", "portico.exe"]
    );
}

fn winget_zips(dir: &std::path::Path, version: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let x64 = dir.join(format!("portico-v{version}-x86_64-pc-windows-msvc.zip"));
    let arm64 = dir.join(format!("portico-v{version}-aarch64-pc-windows-msvc.zip"));
    fs::write(&x64, "x64").unwrap();
    fs::write(&arm64, "arm64").unwrap();
    (x64, arm64)
}

#[test]
fn winget_manifest_renders_version_urls_and_hashes() {
    let dir = TempDir::new().unwrap();
    let (x64, arm64) = winget_zips(dir.path(), "1.2.3");
    let out = dir.path().join("manifests");
    let output = Command::new(script("winget-manifest.sh"))
        .arg("1.2.3")
        .arg(&x64)
        .arg(&arm64)
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let read = |name: &str| fs::read_to_string(out.join(name)).unwrap();
    let version = read("YounesAmalou.Portico.yaml");
    let installer = read("YounesAmalou.Portico.installer.yaml");
    let locale = read("YounesAmalou.Portico.locale.en-US.yaml");
    assert_eq!(fs::read_dir(&out).unwrap().count(), 3);

    for (manifest, kind) in [
        (&version, "version"),
        (&installer, "installer"),
        (&locale, "defaultLocale"),
    ] {
        assert!(
            manifest.starts_with(&format!(
                "# yaml-language-server: $schema=https://aka.ms/winget-manifest.{}.1.9.0.schema.json\n",
                kind
            )),
            "{manifest}"
        );
        assert!(!manifest.contains('@'), "a placeholder is left: {manifest}");
        for expected in [
            "PackageIdentifier: YounesAmalou.Portico\n",
            "PackageVersion: 1.2.3\n",
            &format!("ManifestType: {kind}\n"),
            "ManifestVersion: 1.9.0\n",
        ] {
            assert!(
                manifest.contains(expected),
                "the {kind} manifest lacks {expected}:\n{manifest}"
            );
        }
    }
    for expected in [
        "InstallerType: zip\n",
        "NestedInstallerType: portable\n",
        "- RelativeFilePath: portico.exe\n  PortableCommandAlias: portico\n",
        "- Architecture: x64\n  InstallerUrl: https://github.com/YounesAmalou/Portico/releases/download/v1.2.3/portico-v1.2.3-x86_64-pc-windows-msvc.zip\n  \
         InstallerSha256: 5609F728403E197BB255EF50C62AEABB1F93B09F7B7C379903440B65CD4319CB\n",
        "- Architecture: arm64\n  InstallerUrl: https://github.com/YounesAmalou/Portico/releases/download/v1.2.3/portico-v1.2.3-aarch64-pc-windows-msvc.zip\n  \
         InstallerSha256: F69162950F235E3CDBBAD33F1F912D1A504BE90D8A37D002C735D6F3E3882265\n",
    ] {
        assert!(
            installer.contains(expected),
            "the installer manifest lacks {expected}:\n{installer}"
        );
    }
    for expected in [
        "PackageLocale: en-US\n",
        "License: MIT OR Apache-2.0\n",
        "ReleaseNotesUrl: https://github.com/YounesAmalou/Portico/releases/tag/v1.2.3\n",
    ] {
        assert!(
            locale.contains(expected),
            "the locale manifest lacks {expected}:\n{locale}"
        );
    }
    assert!(version.contains("DefaultLocale: en-US\n"), "{version}");
}

#[test]
fn winget_manifest_refuses_a_zip_of_another_version() {
    let dir = TempDir::new().unwrap();
    let (x64, arm64) = winget_zips(dir.path(), "1.2.2");
    let output = Command::new(script("winget-manifest.sh"))
        .arg("1.2.3")
        .arg(&x64)
        .arg(&arm64)
        .arg(dir.path().join("manifests"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("portico-v1.2.3-x86_64-pc-windows-msvc.zip"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Runs `build-tray.sh` with a fake `cargo` (which writes a binary holding the version it was
/// built with, where the real one would) and a fake `codesign` first on `PATH`.
fn build_tray(dir: &std::path::Path, version: &str, target: &str) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fakes = [
        (
            "cargo",
            r#"#!/bin/sh
case " $* " in *" build --locked --release "*) ;; *) echo "unexpected: $*" >&2; exit 1 ;; esac
while [ $# -gt 0 ]; do
    case $1 in --target) t=$2; shift ;; --target-dir) d=$2; shift ;; esac
    shift
done
mkdir -p "$d/$t/release"
echo "$PORTICO_TRAY_VERSION" > "$d/$t/release/portico-tray"
echo "$PORTICO_TRAY_VERSION" > "$d/$t/release/portico-tray.exe"
"#,
        ),
        (
            "codesign",
            "#!/bin/sh\necho \"$*\" > \"$(dirname \"$0\")/codesign.args\"\n",
        ),
    ];
    for (name, body) in fakes {
        let path = bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    Command::new(script("build-tray.sh"))
        .args([version, target])
        .arg(dir.join("out"))
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .unwrap()
}

#[test]
fn build_tray_requires_a_macos_or_windows_target() {
    let dir = TempDir::new().unwrap();
    let output = build_tray(dir.path(), "0.1.0", "x86_64-unknown-linux-gnu");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("macOS or Windows target required"));
    assert!(!dir.path().join("out").exists());
}

#[test]
fn build_tray_refuses_a_v_prefixed_version_and_needs_three_arguments() {
    let dir = TempDir::new().unwrap();
    let output = build_tray(dir.path(), "v0.1.0", "aarch64-apple-darwin");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no v prefix"));

    let output = Command::new(script("build-tray.sh"))
        .args(["0.1.0", "aarch64-apple-darwin"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("<outdir>"));
}

fn archive_entries(archive: &std::path::Path) -> Vec<String> {
    let listing = Command::new("bsdtar")
        .arg("-tf")
        .arg(archive)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let mut entries: Vec<String> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    entries.sort();
    entries
}

#[test]
fn build_tray_packages_a_signed_bundle_with_the_version_for_macos() {
    let target = "aarch64-apple-darwin";
    let dir = TempDir::new().unwrap();
    let output = build_tray(dir.path(), "0.8.0", target);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let archive = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        archive,
        dir.path()
            .join(format!("out/portico-tray-v0.8.0-{target}.tar.gz"))
    );
    assert_eq!(fs::read_dir(dir.path().join("out")).unwrap().count(), 1);
    assert_eq!(
        archive_entries(&archive),
        [
            "Portico Tray.app/",
            "Portico Tray.app/Contents/",
            "Portico Tray.app/Contents/Info.plist",
            "Portico Tray.app/Contents/MacOS/",
            "Portico Tray.app/Contents/MacOS/portico-tray",
            "Portico Tray.app/Contents/Resources/",
            "Portico Tray.app/Contents/Resources/portico.icns",
        ]
    );
    let codesign = fs::read_to_string(dir.path().join("bin/codesign.args")).unwrap();
    assert!(codesign.starts_with("-s - --force --deep "), "{codesign}");

    let unpacked = dir.path().join("unpacked");
    fs::create_dir(&unpacked).unwrap();
    assert!(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&unpacked)
            .status()
            .unwrap()
            .success()
    );
    let contents = unpacked.join("Portico Tray.app/Contents");
    let plist = fs::read_to_string(contents.join("Info.plist")).unwrap();
    assert!(!plist.contains("@VERSION@"));
    assert_eq!(plist.matches("<string>0.8.0</string>").count(), 2);
    // Built with the version exactly as `portico --version` prints it.
    assert_eq!(
        fs::read_to_string(contents.join("MacOS/portico-tray")).unwrap(),
        "0.8.0\n"
    );
}

#[test]
fn build_tray_zips_portico_tray_exe_alone_for_windows() {
    let target = "x86_64-pc-windows-msvc";
    let dir = TempDir::new().unwrap();
    let output = build_tray(dir.path(), "0.8.0", target);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let archive = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    assert_eq!(
        archive,
        dir.path()
            .join(format!("out/portico-tray-v0.8.0-{target}.zip"))
    );
    assert_eq!(archive_entries(&archive), ["portico-tray.exe"]);
}
