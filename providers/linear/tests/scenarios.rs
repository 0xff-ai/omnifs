//! Data-driven Linear scenarios over the callout tape system.
//!
//! Linear has no prior hand-written test suite (invariant I7 has nothing to
//! preserve here): this is new coverage from zero, following the provider's
//! route table (`providers/linear/src/lib.rs`) directly rather than migrating
//! an inventory of existing assertions.
//!
//! Every callout is a `POST /graphql` carrying a query string plus JSON
//! variables (team key, issue identifier, or pagination cursor), so distinct
//! steps never collide on the tape match key (method + scrubbed url + request
//! body digest): the query text and variables differ per step.
//!
//! ## Recording tenant and privacy
//!
//! Recorded against a real private Linear workspace (`OMNIFS_RECORD_LINEAR_TOKEN`).
//! The target team was chosen by an out-of-band exploratory query for the
//! smallest, least sensitive team available (a small side-project team, not
//! the maintainer's day-job or personal team), to minimize what a checked-in
//! tape exposes.
//!
//! Response bodies use [`BodyPolicy::RewrittenJson`], not the plan's default
//! [`BodyPolicy::Verbatim`]: a live Linear workspace's GraphQL responses carry
//! real names, emails, and free-text titles/descriptions with no way to
//! request a sanitized fixture from the API itself. `sanitize_fields` and
//! `normalize_fields` below are the disclosed, reviewable list of what gets
//! redacted; everything else (team key, issue identifier/number, priority,
//! workflow state name, boolean/pagination scaffolding) stays verbatim
//! because the projection's routing and rendering logic depends on its exact
//! shape and it carries no personal content. `priority` in particular MUST
//! stay out of `sanitize_fields`/`normalize_fields`: it deserializes into
//! `Issue.priority: Option<f64>`, and rewriting a numeric field to a string
//! token would make the rewritten tape fail to parse on replay (the rewritten
//! response body is not just persisted, it is literally what the provider
//! parses when the tape answers a later replay).
//!
//! `name` sanitizes both a private person's identity (`assignee.name`) and a
//! team's display name (`Team.name`), and also catches the shared Linear
//! default workflow-state name (`state.name`, e.g. "Todo"/"Done") because the
//! sanitize mechanism matches by JSON field name at any depth, not by path.
//! Over-redacting the (non-sensitive) workflow state name is an accepted
//! trade-off for keeping the privacy rule simple and field-name-based rather
//! than path-qualified.

use omnifs_itest::scenario::{RecordAuth, Scenario, Step, run};
use omnifs_itest::tape::scrub::{BodyPolicy, TapeRules};

/// The linear mount config the scenarios record against: `api.linear.app` for
/// the GraphQL callouts and a static personal-access-token auth scheme (the
/// `pat` scheme the provider's manifest declares), matching github's
/// `"static-token"`/`"pat"` shape.
const LINEAR_CONFIG: &str = r#"
{
    "provider": "omnifs_provider_linear.wasm",
    "mount": "linear",
    "auth": {
        "type": "static-token",
        "scheme": "pat"
    },
    "capabilities": {
        "domains": ["api.linear.app"]
    }
}
"#;

/// Response-body sanitization for the recorded private workspace.
///
/// `sanitize_fields`: free-text and identity values, redacted to a
/// deterministic per-value token (`<redacted:xxxxxxxx>`), matched at any JSON
/// nesting depth:
/// - `id`: Linear's internal team/issue UUID. Not read by any provider struct
///   (both `Team` and `Issue` ignore it), so redacting is free.
/// - `name`: a team's display name (`Team.name`), a person's name
///   (`IssueAssignee.name`), and Linear's default workflow-state name
///   (`IssueState.name`) all share this JSON key; see the module doc for why
///   the last one is swept up too.
/// - `displayName`, `email`: the assignee's identity fields.
/// - `title`, `description`: issue/team free text.
///
/// `normalize_fields`: fields that churn on every re-record without carrying
/// meaning:
/// - `updatedAt`: the ISO-8601 revalidation timestamp (`Team.version()` /
///   `Issue.version()`), ticks forward on every edit to the recording
///   tenant's workspace.
///
/// Deliberately excluded (kept verbatim, per the plan's "structural fields
/// needed by the projection... may stay verbatim ONLY if they are not
/// sensitive" rule):
/// - `key` (team key): the mount path segment itself, and re-sanitizing it
///   would make `TeamKey`'s validator reject the redacted token (it only
///   accepts alphanumeric/`-`/`_`), silently emptying the `/teams` listing.
/// - `identifier`, `number` (issue identity): the child anchor path segment
///   (`ENG-123`-shaped, explicitly named as safe-to-keep in the plan), needed
///   to route `/teams/{team}/issues/{filter}/{ident}`.
/// - `priority`: numeric (`Option<f64>`); sanitizing would break replay
///   parsing, see the module doc.
/// - `hasNextPage`, `endCursor`: pagination scaffolding. The recording target
///   team has few enough issues that pagination never triggers (single page),
///   so these values are inert; leaving them verbatim also avoids a footgun
///   where a normalized `endCursor` on a real multi-page team would diverge
///   from the real cursor value the next page's recorded *request* carries
///   (only response bodies are rewritten, not request bodies).
const LINEAR_RULES: TapeRules = TapeRules {
    // Linear's rate-limit counters use its own header names, which the base
    // drop list (github-style names) does not cover; the remaining/reset
    // values tick on every request and would churn each re-record. The
    // `-limit` variants and `x-complexity` are constant per query and stay.
    drop_response_headers: &[
        "x-ratelimit-complexity-remaining",
        "x-ratelimit-complexity-reset",
        "x-ratelimit-requests-remaining",
        "x-ratelimit-requests-reset",
    ],
    body: BodyPolicy::RewrittenJson {
        sanitize_fields: &["id", "name", "displayName", "email", "title", "description"],
        normalize_fields: &["updatedAt"],
    },
};

/// Browse the recording team top-down: the provider root (structural, no
/// callout: `/teams` is an auto-navigable literal prefix), a direct lookup of
/// the team by key (routes through the `/teams/{team}` object anchor's own
/// `TEAM_BY_KEY_QUERY` gate, never `teams_list`'s "fetch every team in the
/// workspace" query, so this scenario never records any team's identity but
/// the chosen one), the team's own faces, the two structural state-filter
/// directories (`open`/`all`, computed, no callout), and one issues listing.
#[test]
fn team_browse() {
    run(&Scenario {
        name: "team-browse",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: LINEAR_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_LINEAR_TOKEN",
        }),
        rules: LINEAR_RULES,
        setup: None,
        steps: &[
            Step::List("/"),
            Step::Lookup {
                parent: "/teams",
                name: "PNTR",
            },
            Step::List("/teams/PNTR"),
            Step::Read("/teams/PNTR/item.json"),
            Step::Read("/teams/PNTR/item.md"),
            Step::Read("/teams/PNTR/name"),
            Step::Read("/teams/PNTR/description.md"),
            Step::Lookup {
                parent: "/teams/PNTR/issues",
                name: "open",
            },
            Step::Lookup {
                parent: "/teams/PNTR/issues",
                name: "all",
            },
            Step::List("/teams/PNTR/issues/open"),
        ],
    });
}

/// Browse a single issue's faces: a direct lookup by identifier (routes
/// through the `/teams/{team}/issues/{filter}/{ident}` object anchor's own
/// `ISSUE_BY_IDENTIFIER_QUERY` gate), its directory listing, then every
/// projected face (`item.json`/`item.md` plus the computed `title`/`state`/
/// `priority`/`assignee`/`description.md` fields). The object is `dynamic`,
/// so every face read issues its own `ISSUE_BY_IDENTIFIER_QUERY` fetch (the
/// recorded tape carries one entry per step): the scenario proves each face
/// renders from a fresh load, not warm-canonical reuse.
#[test]
fn issue_read() {
    run(&Scenario {
        name: "issue-read",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: LINEAR_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_LINEAR_TOKEN",
        }),
        rules: LINEAR_RULES,
        setup: None,
        steps: &[
            Step::Lookup {
                parent: "/teams/PNTR/issues/open",
                name: "PNTR-1",
            },
            Step::List("/teams/PNTR/issues/open/PNTR-1"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/item.json"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/item.md"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/title"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/state"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/priority"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/assignee"),
            Step::Read("/teams/PNTR/issues/open/PNTR-1/description.md"),
        ],
    });
}

/// Exercise the engine's revalidating-read op path against a provider that
/// has no conditional-request support at all: unlike github's `revalidation`
/// scenario (which proves `If-None-Match` discrimination), `Issue::load`
/// ignores `since` unconditionally and always issues a full `POST /graphql`
/// fetch (see its doc comment: "Linear's GraphQL has no If-None-Match... we
/// always full-fetch and never return `Load::Unchanged`"). This scenario proves
/// the opposite fact from github's: revalidate still refetches in full, so the
/// tape carries two separate `ISSUE_BY_IDENTIFIER_QUERY` recordings rather than
/// a conditional/304 pair.
#[test]
fn revalidation() {
    run(&Scenario {
        name: "revalidation",
        manifest_dir: env!("CARGO_MANIFEST_DIR"),
        config: LINEAR_CONFIG,
        auth: Some(RecordAuth {
            token_env: "OMNIFS_RECORD_LINEAR_TOKEN",
        }),
        rules: LINEAR_RULES,
        setup: None,
        steps: &[
            Step::Read("/teams/PNTR/issues/open/PNTR-1/item.json"),
            Step::Revalidate("/teams/PNTR/issues/open/PNTR-1/item.json"),
        ],
    });
}
