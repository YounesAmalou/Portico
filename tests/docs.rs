use std::{fs, path::Path};

fn read(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

#[test]
fn required_project_documents_exist() {
    for path in [
        "CHANGELOG.md",
        "CONTRIBUTING.md",
        "SECURITY.md",
        "ROADMAP.md",
        ".github/ISSUE_TEMPLATE/bug_report.md",
        ".github/ISSUE_TEMPLATE/feature_request.md",
        ".github/pull_request_template.md",
    ] {
        assert!(!read(path).trim().is_empty(), "{path} is empty");
    }
}

#[test]
fn roadmap_keeps_every_follow_up_slice_from_the_v1_spec() {
    let spec = read("docs/superpowers/specs/2026-09-13-portico-host-management-design.md");
    let slices = spec
        .split("## Follow-up product slices")
        .nth(1)
        .expect("v1 spec lists follow-up slices");
    let roadmap = read("ROADMAP.md");

    // Every bolded slice title in the spec must survive in the roadmap.
    let titles: Vec<&str> = slices
        .split("**")
        .skip(1)
        .step_by(2)
        .map(|title| title.trim_end_matches(':'))
        .collect();
    assert!(titles.len() >= 6, "expected six slices, found {titles:?}");
    for title in titles {
        assert!(roadmap.contains(title), "roadmap dropped slice {title:?}");
    }

    for planned in [
        "aarch64",
        "macOS",
        "Windows",
        "AUR",
        "Homebrew",
        "end-to-end",
    ] {
        assert!(roadmap.contains(planned), "roadmap lacks {planned}");
    }
}

#[test]
fn changelog_has_the_current_version_section() {
    let heading = format!("## [{}]", env!("CARGO_PKG_VERSION"));
    assert!(read("CHANGELOG.md").contains(&heading));
}

#[test]
fn security_policy_names_a_private_reporting_channel() {
    let policy = read("SECURITY.md");
    assert!(policy.contains("private"));
    assert!(policy.contains("~/.ssh/config"));
}
