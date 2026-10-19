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
fn roadmap_keeps_every_pending_follow_up_slice_from_the_v1_spec() {
    let spec = read("docs/superpowers/specs/2026-09-13-portico-host-management-design.md");
    let slices = spec
        .split("## Follow-up product slices")
        .nth(1)
        .expect("v1 spec lists follow-up slices");
    let (pending, delivered) = slices
        .split_once("### Delivered follow-up slices")
        .expect("v1 spec records which follow-up slices are delivered");
    let roadmap = read("ROADMAP.md");

    fn titles(section: &str) -> Vec<&str> {
        section
            .split("**")
            .skip(1)
            .step_by(2)
            .map(|title| title.trim_end_matches(':'))
            .collect()
    }

    // Every bolded slice title still pending in the spec must survive in the roadmap.
    let pending = titles(pending);
    assert!(
        pending.len() >= 3,
        "expected three slices, found {pending:?}"
    );
    for title in pending {
        assert!(roadmap.contains(title), "roadmap dropped slice {title:?}");
    }

    // A delivered slice leaves the roadmap.
    let delivered = titles(delivered);
    assert!(delivered.contains(&"TUI refresh"), "found {delivered:?}");
    for title in delivered {
        assert!(
            !roadmap.contains(title),
            "roadmap still lists delivered slice {title:?}"
        );
    }

    for planned in ["aarch64", "macOS", "Windows", "AUR", "Homebrew"] {
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
    for job in ["release", "crates", "aur"] {
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
        "can only be created by an administrator",
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
        // The changelog is generated from commit messages; the release workflow only reads it.
        "Conventional Commits",
        "extracts the section",
        // A version bump must keep the lock file in step, or `--locked` builds fail.
        "Cargo.lock",
        "cargo update -p portico-ssh",
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
fn roadmap_records_the_release_follow_ups_and_no_longer_plans_shipped_release_work() {
    let roadmap = read("ROADMAP.md");
    for planned in [
        "Signed releases",
        "Trusted Publishing",
        "crate-scoped token",
    ] {
        assert!(roadmap.contains(planned), "roadmap lacks {planned}");
    }
    assert!(
        !roadmap.contains("First tagged release"),
        "v0.1.0 is released; the roadmap must not wait for it"
    );
    assert!(
        !roadmap.contains("Release automation"),
        "release-plz ships every release; the roadmap must not plan it"
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

#[test]
fn aur_package_builds_the_tagged_source_and_installs_the_binary() {
    let pkgbuild = read("packaging/aur/PKGBUILD");
    for expected in [
        "pkgname=portico\n",
        "license=('MIT' 'Apache-2.0')",
        "depends=('openssh'",
        "makedepends=('cargo')",
        "/archive/refs/tags/v$pkgver.tar.gz",
        "cargo fetch --locked",
        "cargo build --frozen --release",
        "install -Dm755 target/release/portico \"$pkgdir/usr/bin/portico\"",
        "LICENSE-MIT LICENSE-APACHE",
    ] {
        assert!(pkgbuild.contains(expected), "PKGBUILD lacks {expected}");
    }
    assert!(
        !pkgbuild.contains("sha256sums=('SKIP')"),
        "the source archive must be pinned by checksum"
    );

    let srcinfo = read("packaging/aur/.SRCINFO");
    assert!(srcinfo.contains("pkgbase = portico"));
}

#[test]
fn aur_update_script_and_release_steps_are_documented() {
    let update = read("scripts/aur-update.sh");
    assert!(update.contains("makepkg --printsrcinfo"));
    assert!(update.contains("sha256sum"));
    assert!(read("scripts/aur-publish.sh").contains("StrictHostKeyChecking=yes"));

    let contributing = read("CONTRIBUTING.md");
    for explained in [
        "scripts/aur-publish.sh",
        "aur.archlinux.org/portico.git",
        "AUR_SSH_PRIVATE_KEY",
    ] {
        assert!(
            contributing.contains(explained),
            "CONTRIBUTING.md lacks {explained}"
        );
    }
    assert!(read("README.md").contains("yay -S portico"));
}

#[test]
fn aur_and_crates_publishing_are_independent_jobs_with_separate_secrets() {
    let workflow = read(".github/workflows/release.yml");
    let section = |job: &str| {
        workflow
            .split(&format!("\n  {job}:\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("release workflow has no {job} job"))
            .split("\n\n")
            .next()
            .unwrap()
            .to_owned()
    };
    let aur = section("aur");
    let crates = section("crates");
    assert!(aur.contains("environment: aur") && aur.contains("AUR_SSH_PRIVATE_KEY"));
    assert!(crates.contains("environment: crates") && crates.contains("CARGO_REGISTRY_TOKEN"));
    assert!(!aur.contains("CARGO_REGISTRY_TOKEN") && !crates.contains("AUR_SSH_PRIVATE_KEY"));
    for (name, needs) in [("aur", &aur), ("crates", &crates)] {
        assert!(
            !needs.contains("crates]")
                && !needs.contains("aur]")
                && !needs.contains("needs: crates")
                && !needs.contains("needs: aur"),
            "{name} must not depend on the other publishing job"
        );
    }
    assert!(read("packaging/aur/known_hosts").contains("ssh-ed25519"));
}

#[test]
fn changelog_header_matches_what_release_plz_writes_above_each_release() {
    let config = read("release-plz.toml");
    let header = config
        .split("header = \"\"\"")
        .nth(1)
        .and_then(|rest| rest.split("\"\"\"").next())
        .expect("release-plz.toml defines a changelog header");
    let changelog = read("CHANGELOG.md");
    assert!(
        changelog.starts_with(header.trim_end()),
        "CHANGELOG.md must start with the header in release-plz.toml, or release-plz will duplicate it"
    );
}

#[test]
fn release_plz_leaves_publishing_and_the_release_to_the_tag_workflow() {
    let config = read("release-plz.toml");
    for setting in [
        "publish = false",
        "git_release_enable = false",
        "git_tag_name = \"v{{ version }}\"",
        "release_always = false",
        "features_always_increment_minor = true",
    ] {
        assert!(config.contains(setting), "release-plz.toml lacks {setting}");
    }
    assert!(
        config.contains("pr_body") && !config.contains("generated with"),
        "release pull requests carry no release-plz credit footer"
    );
}

#[test]
fn readme_documents_the_connection_test() {
    let readme = read("README.md");
    for expected in ["portico test", "--accept-host-key", "`Ctrl+T`"] {
        assert!(readme.contains(expected), "README lacks {expected}");
    }
}

#[test]
fn readme_documents_adoption() {
    let readme = read("README.md");
    for expected in ["portico adopt", "portico export", "| `m` |"] {
        assert!(readme.contains(expected), "README lacks {expected}");
    }
}

#[test]
fn the_release_pr_body_renders_with_the_pinned_release_plz() {
    // release-plz 0.3.172 renders pr_body with Tera 2; a template it cannot parse stops
    // the release pull request from updating.
    let workflow = read(".github/workflows/release-plz.yml");
    assert_eq!(
        workflow.matches("version: 0.3.172").count(),
        2,
        "both release-plz steps pin the version this test renders for"
    );
    let config = read("release-plz.toml");
    let template = config
        .split("pr_body = \"\"\"\n")
        .nth(1)
        .and_then(|rest| rest.split("\"\"\"").next())
        .expect("release-plz.toml defines pr_body as a multi-line string");
    assert!(
        !template.contains('\\'),
        "pr_body must not rely on TOML escapes"
    );

    let mut tera = tera::Tera::default();
    tera.add_raw_template("pr_body", template)
        .expect("pr_body parses");
    let release = std::collections::BTreeMap::from([
        ("package", "portico-ssh"),
        ("previous_version", "0.3.0"),
        ("next_version", "0.4.0"),
        ("semver_check", "skipped"),
        ("title", "[0.4.0] - 2026-10-19"),
        ("changelog", "### Added\n\n- Filter the containers panel"),
    ]);
    let mut context = tera::Context::new();
    context.insert("releases", &vec![release]);
    let body = tera.render("pr_body", &context).expect("pr_body renders");
    assert!(body.contains("`portico-ssh`: 0.3.0 -> 0.4.0"), "{body}");
    assert!(body.contains("- Filter the containers panel"), "{body}");
    assert!(!body.contains("release-plz"), "no credit footer: {body}");
}
