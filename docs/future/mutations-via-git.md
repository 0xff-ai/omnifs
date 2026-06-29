# Mutations via Git

Status: proposed

## Summary

Each mutable omnifs scope is presented to the user as a Git repository at the mounted path itself.

Users make local changes by editing files in the mount. Those changes remain local until they are added, committed, and pushed. The remote URL uses the `omnifs://` transport, and `git-remote-omnifs` performs reconciliation by loading the same provider configuration and calling the provider's `plan-mutations` and `execute` exports.

Disowned subtrees remain locally writable islands inside the mounted scope. They are visible and editable through the mount, but they are outside the outer repo's reconcile contract. The outer repo ignores them by default and never turns them into provider mutations.

This design supersedes the older transaction-directory and control-namespace mutation model.

## Goals

- Make the mounted mutable scope feel like a normal Git repository.
- Keep mutation execution reconcile-first at the provider boundary.
- Preserve disowned subtree semantics for passthrough trees such as GitHub `repo`.
- Allow local work inside disowned islands, including nested Git repositories.
- Keep local writes local until `git add`, `git commit`, and `git push`.
- Preserve provider ownership of mutation grouping, conflict detection, and post-actions.

## Non-goals

- Making disowned islands part of the outer repo's push semantics.
- Requiring full eager hydration of every provider-owned path before the scope is usable.
- Providing atomic all-or-nothing guarantees across multiple upstream API calls when the upstream system cannot do that.
- Reintroducing direct writable projected files that execute mutations at write time.
- Supporting macOS-specific mount behavior. This design remains Linux-only.

## Why this design

Three user models were considered:

1. A separate state repo outside the mount.
2. A writable mount backed by a separate state repo.
3. The mounted scope itself is the repo.

The third model is the intended UX and the chosen design. It matches normal Git muscle memory:

```bash
cd /github/rust-lang/rust
git status
printf 'closed\n' > issues/42/state
git add issues/42/state
git commit -m "close issue 42"
git push
```

The cost is that the mount must support real Git worktree behavior and must define how provider-owned paths, local overlay state, and disowned passthrough trees coexist. That cost is acceptable because it lands the complexity in the right place: the host and remote helper, not in provider-specific ad hoc mutation paths.

## Terminology

- Mutable scope: a provider-controlled subtree that participates in outer-repo reconciliation. Example: `/github/rust-lang/rust`.
- Outer repo: the Git repository presented at the mutable scope root.
- Provider-owned path: a path inside the mutable scope that belongs to the provider reconcile model.
- Disowned island: a subtree inside the mutable scope that is locally visible and writable but excluded from outer reconcile. Example: `/github/rust-lang/rust/repo`. Note that this reuses the word "disowned" for a writable git-overlay island that does not exist yet. The shipped GitHub provider registers `/{owner}/{repo}/repo` as a read-only `treeref` handoff (`r.treeref("/{owner}/{repo}/repo")`) that the host resolves to a bind-mounted clone (the host-side concept is a subtree, sometimes called `disown-tree`). That shipped subtree is not writable, so this design extends it rather than reusing existing behavior.
- Inner repo: a nested Git repository inside a disowned island.
- Baseline: the last known upstream state for a provider-owned resource, including version tokens.
- Hydration: fetching provider-owned resource files and baseline metadata into local state.
- Post-action: a provider-returned rename, write, delete, or mode change applied after a successful mutation.

## Core decision

The mounted mutable scope is a Git repo from the user's perspective, but it is implemented as a composed FUSE view backed by hidden local state plus live provider access.

The local state has four layers:

1. Disowned backing trees.
2. Outer repo local overlay.
3. Hydrated upstream baseline cache.
4. Live provider browse projection for not-yet-hydrated provider-owned paths.

Read precedence is:

1. Disowned backing tree.
2. Outer repo local overlay.
3. Hydrated baseline.
4. Live provider browse.

This precedence is the narrow invariant that keeps the model coherent:

- If a path is disowned, local filesystem reality wins.
- If a provider-owned path has local edits, those edits win.
- If a provider-owned path has already been hydrated, the local baseline wins over a fresh browse read.
- Live provider browse only fills gaps for provider-owned paths that have not entered local Git state yet.

## User model

### Provider-owned paths

Provider-owned paths participate in outer Git semantics.

Example:

```bash
cd /github/rust-lang/rust
printf 'closed\n' > issues/42/state
git add issues/42/state
git commit -m "close issue 42"
git push origin main
```

Meaning:

- The write changes only local state.
- `git add` stages the provider-owned file in the outer repo.
- `git commit` records a local commit.
- `git push` calls the provider reconcile flow through `git-remote-omnifs`.

### Disowned islands

Disowned islands are writable and visible through the mount, but they do not enter outer reconcile.

Example:

```bash
cd /github/rust-lang/rust/repo
git checkout -b spike
printf '// test\n' >> src/lib.rs
git commit -am "spike"
git push
```

Meaning:

- `repo/` is a disowned island.
- Its reads and writes go to the disowned backing tree.
- Its nested Git semantics are separate from the outer repo.
- The outer repo ignores `repo/` by default.

### Mixed use

Users can work in both layers without switching mental models:

```bash
cd /github/rust-lang/rust
printf 'closed\n' > issues/42/state
git add issues/42/state
git commit -m "close issue 42"

cd repo
git checkout -b release-notes
printf '\nnotes\n' >> RELEASES.md
git commit -am "notes"
git push

cd ..
git push
```

The two pushes are unrelated:

- Inner push uses the inner repo remote.
- Outer push runs provider reconcile for provider-owned paths only.

## Invariants

- A write syscall alone never materializes a remote mutation.
- Only provider-owned paths can become outer-repo mutation inputs.
- Disowned islands are locally visible and writable, but reconcile-invisible to the outer repo.
- The outer repo ignores disowned prefixes by default.
- If a user force-adds a disowned path into the outer repo, outer push fails with a direct diagnostic instead of silently dropping it.
- Providers receive only committed outer-repo changes, never raw write events.
- Providers own mutation grouping, dependency ordering, conflict detection, and post-actions.
- Hydration captures baseline version tokens before a path can participate in outer mutation planning.
- Reads after a successful local write must show the local write through the same mounted path.
- Passthrough subtrees stay on the direct host path once disowned.

## Scope layout

From the user's perspective:

```text
/github/rust-lang/rust/
  .git/
  issues/
  pulls/
  actions/
  repo/          # disowned island
```

Hidden local state on disk:

```text
~/.omnifs/state/github/rust-lang/rust/
  git/
  overlay/
  baseline/
  disowned/
  meta/
```

Recommended hidden state contents:

- `git/`: the outer repo's Git directory.
- `overlay/`: provider-owned local working-tree edits.
- `baseline/`: hydrated provider-owned baseline files and version tokens.
- `disowned/`: backing directories for disowned islands.
- `meta/`: mount identity, provider instance identity, disowned-prefix manifest, resource ownership metadata.

The mounted `.git` directory is a passthrough view of the hidden local `git/` directory. The mounted provider-owned paths are synthesized from `overlay/`, `baseline/`, and live provider browse results. The mounted disowned islands are passthrough views of `disowned/`.

## Path classes

| Path class | Read source | Writable | Outer `git add` | Outer `git push` |
| --- | --- | --- | --- | --- |
| Provider-owned hydrated path | overlay or baseline | yes | yes | yes |
| Provider-owned unhydrated path | provider browse until hydrated | yes | yes | yes |
| Disowned island path | disowned backing tree | yes | ignored by default | no |
| `.git/*` | hidden local Git dir | yes | n/a | n/a |
| `.git/omnifs/*` | hidden omnifs metadata | host-managed | n/a | n/a |

## Hydration model

### Why hydration exists

Provider browse responses are enough to show the tree, but they are not enough to support Git-backed reconcile by themselves. The outer repo needs baseline content and version tokens before a changed provider-owned path can safely participate in mutation planning.

### Hydration trigger

Hydration happens on first mutation-relevant access to a provider-owned resource.

The trigger points are:

- first read of a provider-owned file through the mounted repo
- first write to a provider-owned file
- first provider-owned directory traversal that requires concrete children for Git operations
- explicit refresh via `git pull`

The key design choice is that a first read may hydrate. This is intentional. It makes `git add path` safe even when the path was not already in local baseline state, because Git's file read through FUSE can trigger hydration before the file is staged.

### Hydration operation

Hydration calls `fetch-resource(path)` on the proposed provider reconcile interface (see Provider contract; this interface is not in the shipped WIT yet), where `path` may be any provider-owned path inside the resource. The provider normalizes that path to the owning resource and returns all files for that resource, including version tokens.

Hydration stores:

- resource files in `baseline/`
- version tokens in `meta/`
- file-to-resource ownership metadata in `meta/`
- the hydrated resource snapshot into the outer repo baseline ref

### Resource ownership rule

`fetch-resource(path)` must accept any provider-owned path under a mutable resource and return the full owning resource snapshot.

This is required because the host and remote helper may only know that a user changed `issues/42/title`; they should not have to know independently that the owning resource is `issues/42/`.

## Baseline ref

The design needs a Git-side baseline for diffing committed outer-repo changes against the last known upstream state.

This document keeps the existing conceptual name `upstream`, but the implementation may choose either:

- a visible local branch named `upstream`, or
- a hidden ref such as `refs/omnifs/upstream`

The important invariant is semantic, not naming:

- there is one authoritative baseline ref per outer repo
- outer push computes changes from that baseline ref to `HEAD`
- pull refresh updates that baseline ref first

If the implementation uses a hidden ref, the user-visible branch can remain whatever they choose.

## Read path

### Provider-owned path

On read of a provider-owned path:

1. If the path is under a disowned island, use the disowned backing path.
2. If the path exists in `overlay/`, serve that content.
3. If the path exists in `baseline/`, serve that content.
4. Otherwise browse the provider.
5. If the access is mutation-relevant, hydrate the owning resource before serving.

This preserves current browse behavior while gradually turning accessed provider-owned resources into baseline-backed Git content.

### Disowned path

On read of a disowned path:

1. Resolve the disowned root.
2. Read directly from the disowned backing tree.
3. Do not consult outer repo overlay or provider browse for that subtree.

This requirement extends to the disowned root itself, not just its children. For mounted-repo UX, the disowned root must become backing-path-backed on first contact.

## Write path

### Provider-owned write

On write to a provider-owned path:

1. Resolve whether the path belongs to a disowned island. If yes, use disowned write semantics instead.
2. Hydrate the owning resource if baseline does not exist yet.
3. Apply the write to `overlay/`.
4. Invalidate any provider browse cache entries for that path.
5. Subsequent reads serve the local overlay.

No provider mutation happens here.

### Provider-owned create, rename, delete

Creates, renames, and deletes under provider-owned paths modify the outer repo working-tree view only.

They become candidate provider mutations only after:

- staging,
- committing, and
- pushing.

### Disowned write

On write to a disowned path:

1. Write directly to the disowned backing tree.
2. Never copy the change into outer overlay state.
3. Never stage it automatically in the outer repo.
4. Keep it visible on reread through the same disowned path.

This is the meaning of "not visible to omnifs anyway" in this design: not part of the outer repo's reconcile model, not hidden from local reads.

## Outer Git behavior

### `.git`

Under this design the mounted scope exposes a real `.git` directory backed by hidden local state, and Git commands operate against the mount path itself. The mount is read-only today: the fuser `Filesystem` impl provides only read-side methods (`lookup`, `getattr`, `opendir`, `readdir`, `releasedir`, `read`, `open`, `release`, `readlink`) and no write/create/mkdir/unlink/rename/setattr, so this is a target shape rather than current behavior.

The host must support the file operations Git requires for both `.git` and the outer working tree, including:

- create
- write
- fsync
- rename
- unlink
- mkdir
- rmdir
- chmod and mode changes as needed
- lockfile patterns such as `index.lock` then rename

The current read-only FUSE behavior is not compatible with this design.

### Ignore strategy for disowned islands

Each outer repo writes disowned prefixes into `.git/info/exclude`.

Reason:

- disowned layout is provider-controlled local state
- the ignore rule should not become committed repository content
- inner repos such as `repo/` should stay quiet in outer `git status`

### Force-add safety

Ignore rules prevent normal accidental staging, but they do not stop `git add -f`.

Therefore outer push must validate that `HEAD` and the index contain no tracked entries under disowned prefixes. If they do, push fails with a direct error:

```text
outer push refused: disowned path tracked by outer repo: repo/src/lib.rs
reset or remove the tracked disowned entries, or push from the nested repo instead
```

Rejecting is better than silently ignoring committed changes.

## Inner Git behavior

Disowned islands may themselves contain nested Git repos.

Example:

- `/github/rust-lang/rust/repo/.git` belongs to the source checkout, not the outer omnifs repo.

Rules:

- inner repo reads and writes stay entirely inside the disowned backing tree
- outer repo ignores the disowned prefix
- outer push never interprets inner repo commits
- inner push uses the inner repo remote and is unrelated to outer reconcile

## Remote URL and helper

### URL form

Recommended URL shape:

```text
origin = omnifs://github/rust-lang/rust
```

Other provider examples:

```text
origin = omnifs://dns/cloudflare/example.com
origin = omnifs://linear/my-workspace/team/ENG
```

Git dispatches `omnifs://` to `git-remote-omnifs`.

### Helper responsibility

`git-remote-omnifs` is the only component that turns committed outer-repo changes into provider mutations.

It must:

- locate the mounted scope's provider instance and config
- load the provider component and runtime in headless reconcile mode
- read baseline metadata and version tokens from local repo state
- compute committed provider-owned changes
- call `plan-mutations`
- execute mutations in planned order
- apply provider post-actions to local baseline state
- refresh baseline metadata after success

The helper should link against host runtime code directly instead of talking to the mounted FUSE process over IPC in the first version. Shared daemon state can be added later if cache reuse becomes important.

## Push algorithm

Outer `git push` performs these steps:

1. Resolve the outer repo scope identity from `.git/omnifs/`.
2. Load the provider and runtime in reconcile mode.
3. Validate that no tracked disowned paths exist in the pushed range or index.
4. Compute provider-owned changes from `baseline-ref..HEAD`.
5. Normalize those changes into `file-change` records with `old-content` and `new-content`.
6. Call `plan-mutations(changes)`.
7. Execute planned mutations in dependency order.
8. After each successful mutation:
   - apply post-actions to baseline state
   - advance baseline metadata for affected resources
   - advance the baseline ref for the succeeded portion
9. If all mutations succeed, report success.
10. If a mutation fails, stop and report the exact failed mutation and provider error.

### Partial success

Partial success is possible and must be modeled honestly.

If mutations 1 and 2 succeed and mutation 3 fails:

- upstream side effects for 1 and 2 are already real
- local baseline state advances for 1 and 2
- remaining local commits are not silently rewritten
- the helper reports which mutations succeeded and which failed

The next push computes changes from the advanced baseline and retries only the still-unapplied work.

This is preferable to pretending that provider-side mutation execution is atomic when the upstream API cannot offer that guarantee.

## Pull algorithm

Outer `git pull` performs refresh through the remote helper:

1. Load the provider and runtime in reconcile mode.
2. Call `list-scope(scope-id)` to enumerate mutable resources for the scope.
3. For each resource, call `fetch-resource(resource-path)`.
4. Update the local baseline files and version tokens.
5. Update the baseline ref to the refreshed upstream state.
6. Rebase or merge the current branch onto the refreshed baseline using normal Git behavior.

Conflicts appear as normal Git conflicts in provider-owned files.

Disowned islands are excluded from this refresh.

## Provider contract

This design requires a new provider reconcile interface that does not exist in the shipped WIT contract yet. The current `omnifs:provider` package declares lifecycle, namespace, notify, and callout interfaces, but no mutation-oriented reconcile surface. The reconcile interface this design depends on must add:

- `plan-mutations`
- `execute`
- `fetch-resource`
- `list-scope`

Provider responsibilities:

- interpret committed file changes
- group them into meaningful mutations
- attach dependency ordering with `depends-on`
- use version tokens for compare-and-swap or optimistic concurrency
- return post-actions that reflect server-assigned IDs or fields

Host and helper responsibilities:

- discover changed files
- exclude disowned paths
- manage baseline state and refs
- persist version tokens and resource ownership metadata
- apply post-actions to local state

### `fetch-resource`

`fetch-resource(path)` is the baseline-building primitive.

It must return:

- all files that belong to the owning mutable resource
- the current upstream bytes for those files
- version tokens for conflict detection

### `list-scope`

`list-scope(scope)` is the refresh primitive.

It must enumerate the set of mutable resources that belong to the outer repo scope. Disowned islands are not returned here.

## Post-actions

Post-actions are required for create flows and server-assigned fields.

Examples:

- rename `new-1/` to `42/`
- write `created-at`
- delete a provisional file
- set executable mode if the provider meaningfully projects mode

Post-actions update local baseline state immediately after successful execution.

The current branch update policy is:

- baseline state updates automatically
- user branch history is not silently rewritten by push
- if post-actions changed visible paths or bytes, a later pull or explicit refresh integrates them into the user's branch

This is the most conservative Git-compatible policy for the first version.

## Disowned island detection and persistence

Disowned roots may come from provider materialization and are persisted in local metadata so the outer repo can stay stable across remounts.

The host stores:

- disowned root path
- backing-tree local path
- whether the root contains an inner Git repo

The disowned manifest updates `.git/info/exclude` whenever a disowned root is added or removed.

## Failure modes

### Version mismatch

If `execute` returns `version-mismatch`:

- push fails
- baseline remains advanced only for already-succeeded mutations
- user resolves by pulling and rebasing or by reconciling conflicts manually

### Tracked disowned path

If outer `HEAD` includes tracked entries under a disowned prefix:

- push fails before provider planning
- no provider mutation executes

### Missing baseline metadata

If a provider-owned changed path lacks baseline metadata:

- helper hydrates the owning resource before planning if possible
- if hydration fails, push fails with a direct error

### Inner repo confusion

If a user edits `repo/` and expects outer push to include it:

- outer `git status` should already be quiet because `repo/` is ignored
- if they force-track it, outer push fails with a message directing them to push from the nested repo instead

## Security and correctness

- Provider mutations come only from committed outer-repo state, not from raw write syscalls.
- Disowned islands are a local escape hatch, not a second path into provider mutation execution.
- The helper must validate scope identity from local metadata before loading a provider.
- Baseline version tokens must live in hidden local metadata, not in user-visible projected files.
- Provider-owned and disowned path classes must be deterministic and persisted; a path cannot flip classes within one mounted repo lifetime.

## Host changes implied by this design

This document is not an implementation plan, but it does imply several concrete host responsibilities:

- writable FUSE support for `.git` and provider-owned paths
- repo-scope state management under `~/.omnifs/state/...`
- merged read path across overlay, baseline, provider browse, and disowned backing trees
- hydration on first mutation-relevant access
- persistent disowned-root manifest and ignore-file maintenance
- headless reconcile runtime for `git-remote-omnifs`

The existing read-only mount and child-only disowned lookup behavior are not sufficient for this final shape.

## Alternatives rejected

### Separate state repo outside the mount

Rejected because it splits browsing from mutation and gives the user two places to think about one scope.

### Read-only disowned islands

Rejected because it blocks legitimate local work inside passthrough trees such as nested source checkouts.

### Transaction directories and control namespaces

Rejected because it creates a second mutation protocol once Git is already the intended user model.

## Acceptance criteria

The design is successful when these workflows work as described:

1. `git status` at a mutable scope root works on the mounted path itself.
2. Editing a provider-owned file, then `git add`, `git commit`, and `git push`, creates provider mutations only at push time.
3. Editing a disowned path keeps that edit visible locally but outer `git status` stays clean by default.
4. `git -C /github/rust-lang/rust/repo status` works as a nested repo workflow.
5. Outer push refuses tracked disowned paths with a direct error.
6. `git pull --rebase` refreshes provider-owned baseline state through the provider reconcile interface.

## Open questions

These do not block the design, but they need explicit resolution during implementation:

- Should the baseline ref be a visible `upstream` branch or a hidden `refs/omnifs/upstream` ref?
- Should hydration happen on every first read, or only on first read performed by Git-sensitive access patterns?
- How much of the provider-owned tree should be proactively hydrated at scope open for acceptable `git status` latency?
- Should the helper optionally auto-refresh the current branch after post-actions that renamed paths, or should that always wait for pull?
- Do we want an explicit CLI to inspect disowned manifests and hydration state for debugging?

## Recommendation

Proceed with this model as the repository-level mutation design:

- mounted mutable scope is the outer Git repo
- `omnifs://` plus `git-remote-omnifs` is the only mutation transport
- providers remain reconcile-first
- disowned islands stay locally writable and outer-reconcile-invisible

That gives omnifs one mutation protocol, one user-facing mental model, and one clear boundary for disowned passthrough trees.
