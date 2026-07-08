//! Data-driven github scenarios over the callout tape system.
//!
//! Each scenario records real GitHub HTTP callouts once (via
//! `just host itest-record github <scenario>`) and replays them hermetically in
//! the default host-test lane. Scenarios read namespace-projected files for
//! everything except the repo git tree (`{owner}/{repo}/repo`): that is a
//! subtree boundary served by the OS from a resolved clone, not readable
//! through the namespace read op, so `repo_browse` only lists it (asserting
//! the subtree outcome) after seeding the local clone cache via `setup`;
//! `GitOpenRepo` itself is never a taped callout (plan section 0 non-goal).

use omnifs_itest::RuntimeHarness;
use omnifs_itest::scenario::{RecordAuth, Scenario, Step, run};
use omnifs_itest::tape::scrub::TapeRules;

use crate::support::seed_github_repo_cache;

/// The github mount config the scenarios record against: the `api.github.com`
/// domain for the projection callouts, the `octocat/Hello-World` git remote for
/// the repo-tree subtree step (resolved from the local seeded cache, never a
/// real clone; `GitOpenRepo` is not a taped callout), and a static PAT the
/// recorder authenticates with.
const GITHUB_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_github.wasm",
    "mount": "github",
    "auth": {
        "type": "static-token",
        "scheme": "pat"
    },
    "capabilities": {
        "domains": ["api.github.com"],
        "git_repos": ["git@github.com:octocat/Hello-World.git"]
    }
}
"#;

/// Seeds the local git clone cache for `octocat/Hello-World` so the `/repo`
/// subtree step resolves without a real clone. Runs in both record and replay
/// mode (setup is mode-independent); `GitOpenRepo` always falls through to the
/// real local executor, so this is a fixture, not a tape concern.
fn seed_repo_browse_cache(harness: &RuntimeHarness) {
    seed_github_repo_cache(harness, "octocat", "Hello-World");
}

/// Browse a public repo top-down: the provider root, the owner anchor (owner
/// faces merged with the repo collection), the repo anchor (gated existence plus
/// its static faces), then a read of the repo's canonical JSON out of the object
/// cache the browse warmed. Every HTTP callout is a real recorded GitHub fetch;
/// the trailing `/repo` step resolves the git-tree subtree from the seeded local
/// clone cache (`GitOpenRepo` is never taped) and the final root read exercises
/// the generated `README.md` face, which issues no callout at all.
#[test]
fn repo_browse() {
    run(&Scenario {
        name: "repo-browse",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: GITHUB_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_GITHUB_TOKEN",
        }),
        rules: TapeRules::default(),
        setup: Some(seed_repo_browse_cache),
        steps: &[
            Step::List("/"),
            Step::List("/octocat"),
            Step::List("/octocat/Hello-World"),
            Step::Read("/octocat/Hello-World/repo.json"),
            Step::List("/octocat/Hello-World/repo"),
            Step::Read("/README.md"),
        ],
    });
}

/// Exercise the engine's conditional-revalidation path: a cold read caches the
/// repo canonical with its etag validator, then the revalidating read pushes it
/// back so the provider sends a real `if-none-match` fetch and GitHub answers
/// 304. On replay the conditional header is part of the tape match key, so this
/// scenario proves the plain and conditional fetches resolve to distinct
/// entries.
#[test]
fn revalidation() {
    run(&Scenario {
        name: "revalidation",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: GITHUB_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_GITHUB_TOKEN",
        }),
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Read("/octocat/Hello-World/repo.json"),
            Step::Revalidate("/octocat/Hello-World/repo.json"),
        ],
    });
}

/// Browse GitHub Actions runs: the repo gate, the structural `runs` dir lookup
/// (no fetch), the runs listing, a numeric run-id anchor (structural lookup
/// then a validating listing that preloads `status`/`conclusion`), then reads
/// of those preloaded leaves. Run `22796200342` on `octocat/Hello-World` is a
/// completed, conclusion-`failure` Copilot-review run: terminal state, so the
/// per-run steps stay stable across re-records. The runs listing itself is not
/// pinned to a specific set of ids and will churn on every re-record as new
/// runs land on the repo; that churn is expected and does not affect the
/// per-run steps below it.
#[test]
fn actions_runs() {
    run(&Scenario {
        name: "actions-runs",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: GITHUB_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_GITHUB_TOKEN",
        }),
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::List("/octocat/Hello-World"),
            Step::Lookup {
                parent: "/octocat/Hello-World/actions",
                name: "runs",
            },
            Step::List("/octocat/Hello-World/actions/runs"),
            Step::Lookup {
                parent: "/octocat/Hello-World/actions/runs",
                name: "22796200342",
            },
            Step::List("/octocat/Hello-World/actions/runs/22796200342"),
            Step::Read("/octocat/Hello-World/actions/runs/22796200342/status"),
            Step::Read("/octocat/Hello-World/actions/runs/22796200342/conclusion"),
        ],
    });
}

/// Browse a closed pull request's diff and changed files: the structural PR
/// anchor lookup, the PR listing (faces including `diff.patch`), a read of
/// `diff.patch` (the `FetchBlob` tape arm: `PullRequest::diff` fetches the diff
/// media type and stores it as a blob, not an inline `HttpResponse`), the
/// changed-files listing, and a read of one changed file's markdown face.
/// PR `10194` on `octocat/Hello-World` is closed and was never merged, so its
/// diff, file list, and single changed file (`README`) are terminal and stable
/// across re-records; the `10194` and `README` segments also exercise numeric
/// and literal path-segment routing side by side.
#[test]
fn pr_diff_and_files() {
    run(&Scenario {
        name: "pr-diff-and-files",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: GITHUB_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_GITHUB_TOKEN",
        }),
        rules: TapeRules::default(),
        setup: None,
        steps: &[
            Step::Lookup {
                parent: "/octocat/Hello-World/pulls/all",
                name: "10194",
            },
            Step::List("/octocat/Hello-World/pulls/all/10194"),
            Step::Read("/octocat/Hello-World/pulls/all/10194/diff.patch"),
            Step::List("/octocat/Hello-World/pulls/all/10194/files"),
            Step::Read("/octocat/Hello-World/pulls/all/10194/files/README/file.md"),
        ],
    });
}
