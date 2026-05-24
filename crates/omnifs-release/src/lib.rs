mod changelog;
mod gh;
mod git;
mod npm;
mod semver;
mod workspace;

pub use changelog::{Changelog, finalize_unreleased, unreleased_has_content};
pub use semver::{Version, bump_patch, parse_version};
pub use workspace::{RepoRoot, read_workspace_version, refresh_lockfile, set_workspace_version};

use anyhow::{Context, Result, bail};

pub fn check_changelog_pr(repo: &RepoRoot, base: &str, head: &str) -> Result<()> {
    let changed = git::diff_name_only(repo.root(), base, head)?;
    require_changelog_update(repo, base, &changed)?;
    println!("changelog PR check passed");
    Ok(())
}

pub fn require_changelog_update(repo: &RepoRoot, base: &str, changed: &[String]) -> Result<()> {
    if !changed.iter().any(|path| path == "CHANGELOG.md") {
        bail!(
            "PR must update CHANGELOG.md under ## [Unreleased]; \
             add the no-changelog label to exempt chore-only PRs"
        );
    }

    let base_bytes = git::show_file_at(repo.root(), base, "CHANGELOG.md")?;
    let base_log = Changelog::parse(String::from_utf8_lossy(&base_bytes).into_owned())?;
    let head_log = repo.read_changelog()?;

    if head_log.unreleased_body.trim() == base_log.unreleased_body.trim() {
        bail!("CHANGELOG.md [Unreleased] was not updated");
    }

    if !unreleased_has_content(&head_log) {
        bail!("CHANGELOG.md [Unreleased] must contain release notes");
    }

    Ok(())
}

pub fn check_release_pr(repo: &RepoRoot) -> Result<()> {
    let version = repo.read_workspace_version()?;
    let log = repo.read_changelog()?;
    validate_release_changelog(&log, &version)?;
    npm::validate_synced(repo.root(), &version)?;
    npm::validate_platforms(repo.root())?;
    println!("release PR check passed for version {version}");
    Ok(())
}

pub fn validate_release_changelog(log: &Changelog, version: &str) -> Result<()> {
    let section = log
        .section_for_version(version)
        .with_context(|| format!("CHANGELOG.md missing ## [{version}] section"))?;
    if section.body.trim().is_empty() {
        bail!("CHANGELOG.md ## [{version}] must not be empty");
    }
    if !log.has_unreleased_section() {
        bail!("CHANGELOG.md must include a ## [Unreleased] section");
    }
    if unreleased_has_content(log) {
        bail!("CHANGELOG.md [Unreleased] must be empty in a release PR");
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
pub struct ShipPlan {
    pub should_ship: bool,
    pub version: String,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
}

pub fn ship_plan(repo: &RepoRoot) -> Result<ShipPlan> {
    let version = repo.read_workspace_version()?;
    let latest_tag = git::latest_semver_tag(repo.root())?;
    let should_ship = match latest_tag.as_deref() {
        None => true,
        Some(tag) => {
            let tagged = parse_version(tag.strip_prefix('v').unwrap_or(tag))?;
            let current = parse_version(&version)?;
            current > tagged
        },
    };

    if !should_ship {
        return Ok(ShipPlan {
            should_ship: false,
            version: version.clone(),
            tag: format!("v{version}"),
            release_notes: None,
        });
    }

    let log = repo.read_changelog()?;
    validate_release_changelog(&log, &version)?;
    npm::validate_synced(repo.root(), &version)?;
    npm::validate_platforms(repo.root())?;
    let notes = log
        .section_body_for_version(&version)
        .context("release notes missing")?;

    Ok(ShipPlan {
        should_ship: true,
        version: version.clone(),
        tag: format!("v{version}"),
        release_notes: Some(notes),
    })
}

pub fn create_github_release(repo: &RepoRoot, plan: &ShipPlan) -> Result<String> {
    if !plan.should_ship {
        bail!("refusing to create a release when ship_plan.should_ship is false");
    }
    let notes = plan
        .release_notes
        .as_deref()
        .context("release notes missing from ship plan")?;
    let target_sha = git::head_commit(repo.root())?;
    gh::create_release(
        repo.root(),
        &plan.tag,
        &format!("omnifs v{}", plan.version),
        notes,
        &target_sha,
    )?;
    let tagged = git::resolve_tag_commit(repo.root(), &plan.tag)?;
    if tagged != target_sha {
        bail!(
            "release tag {} points at {tagged}, expected release commit {target_sha}",
            plan.tag
        );
    }
    Ok(tagged)
}

pub fn prepare_release(repo: &RepoRoot, target_version: &str, push: bool) -> Result<()> {
    git::ensure_clean_tree(repo.root())?;
    git::ensure_on_branch(repo.root(), "main")?;

    let current = repo.read_workspace_version()?;
    let current_v = parse_version(&current)?;
    let target_v = parse_version(target_version)?;
    if target_v <= current_v {
        bail!(
            "target version {target_version} must be greater than current workspace version {current}"
        );
    }

    let log = repo.read_changelog()?;
    if !unreleased_has_content(&log) {
        bail!(
            "CHANGELOG.md [Unreleased] is empty; add release notes on main before cutting a release"
        );
    }

    let branch = format!("release/v{target_version}");
    git::create_branch(repo.root(), &branch)?;

    repo.set_workspace_version(target_version)?;
    repo.refresh_lockfile()?;
    npm::sync_versions(repo.root(), target_version)?;

    let mut log = repo.read_changelog()?;
    finalize_unreleased(&mut log, target_version)?;
    validate_release_changelog(&log, target_version)?;
    std::fs::write(repo.changelog_path(), &log.raw)?;
    npm::validate_synced(repo.root(), target_version)?;
    npm::validate_platforms(repo.root())?;

    let message = format!("release: v{target_version}");
    git::commit_all(repo.root(), &message)?;
    if push {
        git::push_branch(repo.root(), &branch)?;
        gh::create_pull_request(repo.root(), &branch, target_version)?;
    }

    println!("prepared release v{target_version} on branch {branch}");
    Ok(())
}

pub fn release_notes_prompt(repo: &RepoRoot) -> Result<String> {
    let latest = git::latest_semver_tag(repo.root())?;
    let range = match latest.as_deref() {
        Some(tag) => format!("{tag}..HEAD"),
        None => "HEAD".to_string(),
    };

    Ok(format!(
        r"# Release notes prompt

Write a Keep a Changelog `## [Unreleased]` section for omnifs from the commit range below.
Inspect the repo with git (log, diff, show) as needed. Use end-user language, merge related
changes, and omit internal-only refactors unless they affect users. Use `### Added`,
`### Changed`, and `### Fixed` where appropriate.

Return only the markdown body for `[Unreleased]` (subsection headings and bullets). Do not
include the `## [Unreleased]` heading itself.

## Commit range

{range}
"
    ))
}

impl RepoRoot {
    pub fn find(start: impl AsRef<std::path::Path>) -> Result<Self> {
        workspace::find_repo_root(start)
    }

    pub fn read_changelog(&self) -> Result<Changelog> {
        Changelog::parse(std::fs::read_to_string(self.changelog_path())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_release_changelog_rejects_nonempty_unreleased() {
        let log = Changelog::parse(
            "# Changelog\n\n\
             ## [Unreleased]\n\n\
             - pending\n\n\
             ## [0.2.0] - 2026-01-01\n\n\
             ### Added\n\n\
             - shipped\n"
                .to_string(),
        )
        .unwrap();
        let err = validate_release_changelog(&log, "0.2.0").unwrap_err();
        assert!(err.to_string().contains("[Unreleased]"));
    }
}
