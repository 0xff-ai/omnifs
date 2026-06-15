# Docs voice and tone

Guidance for anyone (human or agent) writing or editing pages in the omnifs docs. The structure these pages slot into is described in [`README.md`](README.md), with the section order and labels in [`_nav.ts`](_nav.ts). This file governs how they read.

## The house voice

Three things hold everywhere:

- **Concrete over abstract.** Name the thing. "A GitHub issue is one JSON document" beats "resources have backing representations." If a sentence would survive being deleted, delete it.
- **One idea per paragraph.** Lead with the idea, then support it. No throat-clearing, no "in this section we will."
- **Honest about limits.** State what is true today, name what is not done, never oversell a guarantee. "Traces redact common secrets. They do not redact everything."

No em dashes (use parentheses, colons, or separate sentences). Sentence case in headings. American spelling.

## The four registers

Each page sits in one register. Match it; do not blend.

### 1. Concept

Orientation and architecture pages: Get oriented, Projections, Engine, the section intros. Plainspoken and vivid, never decorative. This is the bar; the strongest existing pages already sit here.

```
Some resources have a single upstream truth behind them. A GitHub issue is one
JSON document; from it you read a title, a body, a state, and a rendered
`item.md`. omnifs calls that document the object, and the files beside it are
projected from it. Read one and the rest cost nothing, because they all come
from the same payload the provider already fetched.
```

### 2. Procedure

Install, mounting, tutorials. Bare imperative steps. Numbered when order matters. No prose between steps unless a step needs a caveat.

```
1. `omnifs init github`
2. `omnifs up`
3. `omnifs shell`

If `gh auth token` is available, init offers to reuse it.
```

### 3. Recipe

The cookbook. Command first, then one line of consequence. No preamble.

```
## Grep across issues

    grep -rl "panic" /omnifs/github/rust-lang/rust/issues/open

Each issue is a real file, so `-r` walks them like any directory. Matches are
fetched as you descend; nothing is downloaded ahead of time.
```

### 4. Security

Security and trust pages. Clinical, confident, caveated. State the boundary plainly, then state where it does not hold.

```
omnifs gives you two views into what the runtime did: `omnifs inspect`, which
traces a single read end to end, and the runtime log. Both show the host's
actions, not the provider's intent.

Traces redact common secrets in URLs, headers, and git remotes. They do not
redact everything. A trace can still carry paths, object names, and provider
output, so treat sharing one as a disclosure decision.
```

## Vocabulary

Do not define terms here. The source of truth is the repo-root `AGENTS.md`, which
defines provider, host, mount, projection, render, callout, effects, the object /
view / blob caches, and the rest. Its standing rule applies to docs too: reuse
source-of-truth terms, do not coin synonyms, and do not rename a public surface
without an explicit decision. The docs Glossary page is the terminology registry;
copy reviews check prose against it.

Two terms are easy to misuse in prose, so state them exactly:

- **render** means canonical object to a format (markdown, yaml, json), nothing
  wider. The principle that the projection is independent of the frontend serving
  it is **frontend agnosticity**.
- **mount** means binding one provider to a path prefix (`/github`). For the read
  surface and the filesystem view, use **projection**; for how it reaches the OS,
  use **surface** (FUSE, NFSv4, FSKit).

## Adapting existing prose

When prose comes in accurate but flat, tighten it into the register above: cut hedging, lead with the idea, make examples concrete. Tighten, do not flatten, and never trade a correct caveat for a cleaner sentence. Reference pages stay terse and tabular; they are generated from source, so prose there is minimal.

## Honesty gates

Non-negotiable for every page. These outlived the sitemap exploration that produced them; treat them as standing constraints.

- **Read-only today.** Writes land later as reviewable, git-shaped diffs. Never imply a projected file is writable.
- **Mount is Linux-only.** The FUSE mount runs on Linux; macOS and Windows run it inside a Linux container shell. Do not claim a native macOS FUSE mount. Host-native NFSv4/FSKit surfaces are the roadmap direction (host-native NFS already works on macOS); label them as such.
- **No unproven numbers.** Token-savings and benchmark figures appear only when reproducible. Until then phrase the win as "less context to hand an agent," never a measured number.
- **`worldview` is roadmap vocabulary** until its spec ships; keep it in Roadmap sections, not in the present-tense surface.
- **Traceability means local observability** (the inspector), not a compliance audit.
- **Code wins on drift.** When a README or design doc disagrees with the code, the code is right; fix the prose.

## Generation principle

Reference is generated, prose is written. Reference pages derive from source (CLI from clap, config and manifest from the schema types, WIT from `provider.wit`, control API from the OpenAPI derivation, per-provider pages from each `omnifs.provider.json` plus its route table); do not hand-maintain facts a generator owns. Concepts, guides, and the cookbook are hand-written. Every command block in a quickstart or recipe runs in CI against a real mount: runnable or deleted.

## Structural rules

The information architecture in [`_nav.ts`](_nav.ts) holds two invariants:

- **One concept, one home.** A concept lives on a single page in a single section. Do not shred an essay across sections or hedge with "two altitudes" duplicates.
- **No page name repeats** across sections.

Naming knots already resolved (keep them resolved):

- The host mechanism (authorize, execute, resume, commit) is **Callouts and effects** under Engine. The provider-authoring view of the same machinery is **Reaching upstream** under Providers. Do not collapse them or duplicate the name.
- File attributes have one concept home: **What files report** under Projections (what a reader and the toolbox see). Declaring attributes is part of the Providers authoring guide; the exact enum table is generated Reference. No standalone duplicate.
- Capabilities have one concept home in Engine (the kinds and the check). The grantable list is generated Reference; there is no separate "Capability types" concept page.
