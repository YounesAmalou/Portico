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

#[test]
fn manual_runs_never_release_or_publish() {
    let workflow = read(".github/workflows/release.yml");
    for job in ["release", "publish"] {
        let section = workflow
            .split(&format!("\n  {job}:\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("release workflow has no {job} job"))
            .split("\n\n")
            .next()
            .unwrap();
        assert!(
            section.contains("github.event_name == 'push'")
                && section.contains("github.ref_type == 'tag'"),
            "{job} must require a pushed tag, not a manual run on a tag ref"
        );
    }
}

#[test]
fn security_policy_describes_the_include_edit_accurately() {
    let policy = read("SECURITY.md");
    assert!(policy.contains("inserts an `Include` line as the first line"));
    assert!(!policy.contains("rewrites the first line"));
}

#[test]
fn contributing_documents_every_workflow_and_how_it_is_triggered() {
    let contributing = read("CONTRIBUTING.md");
    let workflows = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    for entry in fs::read_dir(workflows).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            contributing.contains(&name),
            "CONTRIBUTING.md does not document workflows/{name}"
        );
    }
    for explained in [
        "pull request",
        "pushing a `vX.Y.Z` tag",
        "manual run",
        "never creates a release or publishes",
    ] {
        assert!(
            contributing.contains(explained),
            "CONTRIBUTING.md does not explain: {explained}"
        );
    }
}

#[test]
fn actions_are_pinned_to_commit_shas() {
    let workflows = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    for entry in fs::read_dir(workflows).unwrap() {
        let path = entry.unwrap().path();
        for line in fs::read_to_string(&path).unwrap().lines() {
            let Some(reference) = line
                .trim()
                .strip_prefix("- uses: ")
                .or_else(|| line.trim().strip_prefix("uses: "))
            else {
                continue;
            };
            let reference = reference.split_whitespace().next().unwrap();
            if reference.starts_with("./") {
                continue;
            }
            let (_, revision) = reference.split_once('@').unwrap_or_else(|| {
                panic!("{} uses {reference} without a revision", path.display())
            });
            assert!(
                revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
                "{} must pin {reference} to a full commit SHA",
                path.display()
            );
        }
    }
}

#[test]
fn dependabot_keeps_actions_and_cargo_dependencies_current() {
    let config = read(".github/dependabot.yml");
    assert!(config.contains("package-ecosystem: github-actions"));
    assert!(config.contains("package-ecosystem: cargo"));
}

#[test]
fn contributing_explains_who_can_publish() {
    let contributing = read("CONTRIBUTING.md");
    for explained in [
        "requires the maintainer's approval",
        "`v*` tags are protected",
        "pinned to commit SHAs",
        "Dependabot",
    ] {
        assert!(
            contributing.contains(explained),
            "CONTRIBUTING.md does not explain: {explained}"
        );
    }
}

#[test]
fn contributing_describes_the_changelog_and_every_release_step() {
    let contributing = read("CONTRIBUTING.md");
    for explained in [
        // The changelog is written by hand; the workflow only reads it.
        "written by hand",
        "extracts the section",
        // A version bump must keep the lock file in step, or `--locked` builds fail.
        "Cargo.lock",
        "cargo update -p portico-ssh",
        // The publish job waits for a human.
        "Review deployments",
        // The settings that protect the release are recorded so they can be recreated.
        "release tags are immutable",
        "only admins create release tags",
        "CARGO_REGISTRY_TOKEN",
        "private vulnerability reporting",
    ] {
        assert!(
            contributing.contains(explained),
            "CONTRIBUTING.md does not explain: {explained}"
        );
    }
}

#[test]
fn roadmap_records_the_release_follow_ups_and_no_longer_waits_for_the_first_release() {
    let roadmap = read("ROADMAP.md");
    for planned in [
        "Signed releases",
        "Trusted Publishing",
        "Release automation",
        "crate-scoped token",
    ] {
        assert!(roadmap.contains(planned), "roadmap lacks {planned}");
    }
    assert!(
        !roadmap.contains("First tagged release"),
        "v0.1.0 is released; the roadmap must not wait for it"
    );
}

#[test]
fn the_publish_plan_records_where_the_work_deviated() {
    let plan = read("docs/superpowers/plans/2026-10-13-publish-foundation.md");
    for deviation in [
        "rust-version = \"1.88\"",
        "ratatui",
        "non-root",
        "manual run",
    ] {
        assert!(
            plan.contains(deviation),
            "the plan does not record the deviation: {deviation}"
        );
    }
}
