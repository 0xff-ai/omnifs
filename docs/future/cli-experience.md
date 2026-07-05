# CLI experience masterplan

Status: companion to the bluesky CLI rework plan, which is no longer tracked in the repo. That plan fixes the CLI's structure (module ownership, backend identity, command hygiene); this one defines the experience the structure serves. Items marked (WS3)/(WS4)/(WS5) ride those workstreams; items marked (P2) are the post-rework phase and must not be scaffolded early.

## North star

The mount is the product. `cat`, `grep`, and `ls` are the real interface; the CLI exists to get a user to a working mount fast, keep them truthfully informed, and get out of the way. Calibration points: `tailscale` (daemon lifecycle UX: `up` converges, `status` is glanceable), `gh` (bare invocation is useful, JSON everywhere), `brew doctor` (triage that names the fix). The one metric that rules the rest: **time to first cat**.

Principles, in priority order:

1. **A guided front door and an express lane, same destination.** `omnifs setup` is the wizard: orientation, environment checks, choices explained in consequence terms, the first mount, live verification, graduation. `omnifs init <provider>` is the express lane for someone arriving from a provider README: defaults with a notice, no detour. Both are narrations over the same stage implementations, so neither can drift from the other.
2. **Never dead-end.** Every failure names the next command. A user or agent holding an error message is holding instructions.
3. **Two first-class audiences, one surface.** Every command has human TTY output and `--json`; every prompt has a flag; exit codes are a stable contract. Agents are not a special mode, they are half the users.
4. **Honest state, quiet by default.** One source of truth per fact (the daemon when alive); no guessing, no wall-of-text; detail behind `-v` and `--detail`.
5. **Trust moments are explicit.** Capability grants and auth scopes get a consent screen every time they widen, never buried in verbosity (the existing init grant screen stays; the daemon-enforced upgrade consent from WS4 is its backend).

## The four golden paths

### 1. First run: two doors, one destination

**The front door: `omnifs setup`, the guided wizard.** For the first-timer with no mental model, about to grant capabilities and hand over credentials. It orients, checks, explains each choice in consequence terms, gets a first mount working, and proves it live:

```
$ omnifs setup
  omnifs projects services (GitHub, Linear, arXiv, ...) into your filesystem
  as regular files, served by a local daemon at ~/omnifs.

  1/6  environment    macOS 15 · Docker 27.1 running ✓
  2/6  runtime        Docker (recommended) — native NFS is experimental    [enter]
  3/6  mount point    ~/omnifs                                             [enter]
  4/6  first mount    › arxiv — no credentials needed
                        github — device-flow auth
                        linear — API key
  5/6  auth + grants  (per provider: the capability screen, then the auth flow)
  6/6  launch         ✓ daemon started · ✓ serving

  $ ls ~/omnifs/arxiv          ← ran for you
  by-id/  search/  new/

  You're set. Daily commands:
    omnifs             status at a glance
    omnifs doctor      when something looks wrong
    omnifs init <p>    add another mount
```

Target: a few minutes, and the user leaves knowing the model, having seen a real listing, holding exactly three commands.

**The express lane: `omnifs init <provider>`.** For the person arriving from a provider README, and for every mount after the first:

```
$ omnifs init github
  omnifs isn't set up yet — using defaults (Docker runtime on macOS).
  For the guided tour, run `omnifs setup` instead.

  github will be able to reach:
    network   api.github.com, github.com (HTTPS)
  auth: GitHub device flow

  ? Continue [Y/n]
  → code A1B2-C3D4 copied; opening https://github.com/login/device ...
  ✓ authenticated as raulk (repo:read)
  ✓ serving: ~/omnifs/github          try: ls ~/omnifs/github/raulk
```

Targets: ≤ 90 seconds with OAuth, ≤ 15 seconds with an ambient token, ≤ 2 prompts, zero prerequisite commands.

Both doors run the same stage implementations (environment check, runtime selection, spec creation, auth, launch, verify); the wizard narrates them one screen at a time, the express lane compresses them behind defaults. A fix to a stage fixes both.

### 2. The daily glance: bare `omnifs` is the dashboard

```
$ omnifs
  up (native) — ~/omnifs — 3 mounts
    github   ✓ healthy    auth ✓ expires 54d
    linear   ✓ healthy    auth ✓
    arxiv    ✓ healthy    no auth
  1.2k ops last hour, 0 errors
```

Bare invocation prints the compact dashboard when configured, the onboarding pointer when not. `omnifs status` is the same view; `--detail` adds capabilities, versions, cache; `--json` is the machine form. When the daemon is down, status still renders configured mounts plus `daemon not running — run: omnifs up`. Health and expiry come from `CredentialService::health()` through the API, not local file arithmetic (WS3, WS4).

### 3. Failure triage: from EIO to the fix in one command

The mount can only say `EIO`; the CLI must close the gap:

```
$ cat ~/omnifs/github/raulk/omnifs/README.md
cat: ...: Input/output error
$ omnifs doctor
  ✗ github: credential rejected upstream (invalid_token, 2h ago)
      fix: omnifs mounts reauth github
  ✓ daemon, frontend, providers, network, credential store
```

- **Doctor becomes triage, not just preflight.** Today it never talks to the daemon; it gains a live section that correlates daemon health, credential states, and the recent inspector tail into per-mount verdicts with a named fix (WS4 slices 4-5, WS5).
- **Nudges.** Any state-touching command prints at most one stderr line per day when something is degraded or expiring (`note: github credential expires in 2 days — omnifs mounts reauth github`), rate-limited via a timestamp in the workspace. Proactive refresh (WS3) makes these rare; the nudge covers what refresh cannot fix (revocation, `NeedsConsent`).
- **`omnifs doctor --fix`** (P2): safe auto-repairs only — stale launch record, orphaned NFS mounts, missing provider artifacts — each printed before it runs.

### 4. The agent path: flags, JSON, exit codes

```
$ omnifs init github --no-input --token-env GITHUB_TOKEN --yes
$ omnifs status --json | jq -e '.mounts[] | select(.health != "healthy")'
$ omnifs up --wait 30s
```

- `--json` on every reporting command (`status`, `doctor`, `mounts ls`, `providers ls`, `version`); JSON shapes come from `omnifs-api` types, so the CLI and REST answers can never diverge (WS5).
- `--no-input` never hangs: a flow that would prompt fails immediately, naming the exact flag that satisfies it.
- **Exit code contract**, documented in `--help`: 0 success; 1 failure; 2 usage; 3 daemon unreachable; 4 auth or consent required; 5 degraded (command succeeded, something needs attention).
- `omnifs up --wait <dur>` blocks until the frontend serves or fails; the scripted equivalent of watching the spinner.
- stdout is data, stderr is progress/notices/hints; spinners and color only on a TTY; `NO_COLOR` honored.

## The setup wizard, specified

The wizard is the flagship starting experience, not a config script. Its job at the moment of zero mental model: orient, build justified trust, produce a working mount, and hand over exactly the vocabulary needed for daily life. Today's `omnifs setup` already has the right skeleton (OS detect → runtime picker → provider picker → init loop → summary → up); this upgrades it from configuration to onboarding.

**Stages** (numbered on screen, `n/6`, so the commitment is visible):

1. **Orientation.** Three sentences: services become files, a local daemon serves them at one mount point, the CLI manages it. No theory beyond that; the rest is taught at the moment it applies.
2. **Environment.** OS, Docker present and running (when relevant), FUSE availability on Linux, existing-workspace detection. Failures offer the fix inline and wait ("Docker isn't running — start it and press enter, or choose native"), never restart the wizard.
3. **Runtime.** Docker vs native explained in consequence terms (macOS: Docker recommended, native NFS experimental; Linux: native FUSE default). Default preselected, enter accepts. Persists `[system].runtime`.
4. **Mount point.** Shown, confirmable.
5. **First mount.** The provider picker with one-line manifest descriptions; credential-free providers (arxiv, dns) flagged "no credentials needed" as the low-friction first win. Then the per-provider guided init: the capability consent screen, the auth flow with its explain prose, config fields with defaults.
6. **Launch and prove it.** Start the daemon, wait for serving, then run a real listing of the new mount and show the output inside the wizard. The wizard never claims success it hasn't demonstrated.

**Graduation card** at the end: what's mounted where, the three daily commands (`omnifs`, `omnifs doctor`, `omnifs init <provider>`), and an offer to install shell completions.

**Properties**:

- **Idempotent and resumable.** Every stage derives its state from the workspace; Ctrl-C is safe, re-running continues where things stand. No wizard-state file.
- **Re-running is review mode.** On a configured system, `omnifs setup` becomes the settings surface: show current runtime/mount point/mounts, change any of them, re-run the environment checks, add providers. This gives the wizard a permanent job, which is what justifies polishing it.
- **Every answer has a flag**; `setup -y` accepts all defaults end to end (exists today; stays).
- **Shared stages, not a parallel implementation.** The wizard is narration and sequencing over the same stage modules `init`, `up`, and `doctor` use (the WS5 thin-command restructure is what makes this true; today `setup/mod.rs` embeds 461 lines of its own logic).
- **Trust moments teach.** Capability grants and auth scopes are where the security model gets explained, one screen each, at the point of decision.

## Command grammar (final)

```
omnifs                        the dashboard (or onboarding pointer)
omnifs setup                  the guided front door: wizard on first run, settings review on re-run
omnifs init <provider> [--as] the one-shot: spec + auth + daemon + verified mount
omnifs up | down | status | doctor | logs | shell | inspect | reset | version | completions
omnifs mounts ls | rm <name> | reauth <name>
omnifs providers ls | show <name> | add <path>
omnifs daemon                 hidden; the runtime loop
omnifs debug ...              hidden; offline introspection
```

Rules the grammar obeys:

- **One canonical spelling per action.** `mounts add` is gone (`init` is creation); re-auth is a mounts verb, not an `init` mode with a repurposed positional.
- **Lifecycle verbs are top-level, resources are noun groups.** No `omnifs auth` namespace (settled decision: init/reauth own credential acquisition).
- **Consistent flag semantics.** `--json` means the same everywhere; `--detail` means "more rows/columns of the same view", never a different view; `-y/--yes` accepts defaults, `--no-input` forbids prompts.
- **Completions are dynamic**: mount names and provider names complete from the workspace (P2 polish).

## Provider help comes from the manifest

The provider manifest already declares auth schemes, guidance prose, capabilities, and config fields; the CLI renders it instead of duplicating it:

- `omnifs providers ls` — installed and available providers with one-line descriptions.
- `omnifs providers show github` — capabilities it will request, auth flows it supports (from `SchemeGuidance`), config fields with defaults, current pinned version. This is the pre-consent research surface, generated entirely from `ProviderManifest` (one authority; a provider README change cannot drift from what the CLI shows).

## `omnifs try <provider>` — the zero-friction demo (candidate, recommend)

`arxiv` and `dns` need no credentials. `omnifs try arxiv` mounts into an ephemeral workspace (temp home, auto-cleanup on exit or `try --end`), prints the path, and tears down cleanly. Time-to-wow under 30 seconds with zero persistent state and zero consent friction beyond the capability screen. This is the demo loop for talks, READMEs, and first contact. Additive; needs product sign-off on the ephemeral-workspace shape before building.

## In-mount legibility (P2, explicitly out of the rework's scope)

An agent that can only `cat` should not need the CLI for health: a per-mount `@health` synthetic file and a root-level orientation file, fed by the same health tables the API serves. The `@`-name reservation and the ignore-file mechanism (which hides control names from `find`/`grep -r`) already exist for pagination, so the affordance pattern is proven. This lands only after WS3/WS4 make health a real, tested fact; building it earlier would violate the no-empty-seams rule, and it must respect the product contract (standard tools see a normal tree unless they ask).

## Measurement and acceptance

- **Time to first cat**: ≤ 90s OAuth / ≤ 15s ambient, measured by a scripted golden-path e2e test (setup defaults → `init test` with the fixture provider and fake OAuth server → verified read), running headless in CI.
- **Prompt budget**: golden path ≤ 2 prompts; `--no-input` covers 100% of prompts (asserted by a flag-coverage test over the clap tree).
- **Hint coverage**: every `ApiError` code and every CLI-local failure maps to a hint in one table; a unit test fails on unmapped codes.
- **JSON coverage**: every reporting command has `--json` backed by an `omnifs-api` type.
- **Exit codes**: asserted in the e2e suite (kill the daemon → 3; revoke the fixture credential → 4).

## Sequencing

| Wave | Items | Rides |
|---|---|---|
| 1 | grammar cleanup, `--json` everywhere, exit-code contract, hint table over `ApiError`, stdout/stderr discipline, setup stage extraction (wizard and init as narrations over shared stages) | WS5 (needs WS4 slice 1 for error codes) |
| 2 | dashboard bare invocation, status/doctor live health, nudges, reauth live-apply UX, wizard live-verify + graduation card + review mode | WS3 + WS4 slices 4-5 |
| 3 | one-shot `init` (implied setup + up + verify), `up --wait`, `providers show`, golden-path e2e in CI | WS4 slice 2, WS5 |
| 4 (P2) | `doctor --fix`, dynamic completions, `omnifs try`, in-mount `@health` | post-rework |
