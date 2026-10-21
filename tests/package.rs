use std::{fs, path::Path, process::Command};

fn read(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

#[test]
fn package_is_published_as_portico_ssh_under_a_dual_license() {
    assert_eq!(env!("CARGO_PKG_NAME"), "portico-ssh");
    assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
}

#[test]
fn installed_binary_is_named_portico() {
    let output = Command::new(env!("CARGO_BIN_EXE_portico"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("portico {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn license_files_name_the_copyright_holder() {
    let mit = read("LICENSE-MIT");
    assert!(mit.contains("Younes A. Amalou"));
    assert!(mit.contains("Permission is hereby granted"));

    let apache = read("LICENSE-APACHE");
    assert!(apache.contains("Apache License"));
    assert!(apache.contains("Version 2.0"));
}

#[test]
fn published_crate_contains_only_intended_files() {
    let output = Command::new(env!("CARGO"))
        .args(["package", "--list", "--allow-dirty", "--offline"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let files = String::from_utf8(output.stdout).unwrap();
    let files: Vec<&str> = files.lines().collect();

    // The binary embeds the desktop panel and the launcher entry, so the crate carries them.
    for expected in [
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "README.md",
        "src/main.rs",
        "desktop/quickshell/shell.qml",
        "desktop/quickshell/manifest.json",
        "packaging/desktop/portico.desktop",
        "packaging/desktop/portico.svg",
    ] {
        assert!(
            files.contains(&expected),
            "{expected} missing from {files:?}"
        );
    }
    // Repository-hygiene tests read files that are not published, so they must not ship either,
    // nor do the panel's tests and preview image, or the AUR and Arch packaging.
    for forbidden in [
        ".superpowers",
        ".worktrees",
        "target/",
        "docs/superpowers",
        "packaging/aur",
        "packaging/arch",
        "desktop/quickshell/tests",
        "desktop/quickshell/preview.png",
        "tests/docs.rs",
        "tests/package.rs",
    ] {
        assert!(
            !files.iter().any(|file| file.starts_with(forbidden)),
            "{forbidden} must not be published: {files:?}"
        );
    }
}
