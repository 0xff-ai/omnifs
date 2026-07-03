#!/usr/bin/env bun
// Deterministic generator for the agentbench fixture corpus.
//
// Produces ~200 small files under `fixture-data/` across a github-like tree
// (`github/repos/<repo>/issues/<n>/{body.md,comments/}`) and a linear-like tree
// (`linear/issues/<KEY>/{issue.md,activity.md}`). Facts are planted at fixed
// locations so the tasks in `tasks/*.yaml` have stable, gradeable answers.
//
// The planted facts (and therefore the task answers) live in `FIXTURE-FACTS.md`
// next to this script, deliberately OUTSIDE `fixture-data/` so an agent reading
// the corpus never sees the answer key. Regenerate with `bun gen-fixture.ts`.

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const root = join(scriptDir, "fixture-data");

let fileCount = 0;
function write(relPath: string, body: string): void {
  const abs = join(root, relPath);
  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, body.endsWith("\n") ? body : `${body}\n`);
  fileCount += 1;
}

// Deterministic pseudo-word picker (no Math.random): index into fixed pools.
const TITLE_POOL = [
  "Fix flaky retry backoff",
  "Document the mount lifecycle",
  "Reduce cold-start latency",
  "Handle empty listing responses",
  "Cache invalidation drops stale entries",
  "Support ranged reads on large files",
  "Propagate provider errors to the CLI",
  "Add structured logging to the daemon",
  "Guard against duplicate reconcile runs",
  "Trim the release image footprint",
];
const LABEL_POOL = ["bug", "enhancement", "docs", "perf", "infra"];
const AUTHOR_POOL = ["dvora", "ken", "mira", "sol", "wen", "amir"];

function title(n: number): string {
  return TITLE_POOL[(n * 7 + 3) % TITLE_POOL.length];
}
function label(n: number): string {
  return LABEL_POOL[(n * 5 + 1) % LABEL_POOL.length];
}
function author(n: number): string {
  return AUTHOR_POOL[(n * 3 + 2) % AUTHOR_POOL.length];
}

interface RepoSpec {
  name: string;
  issues: number;
  // comment count per issue number (1-indexed); index 0 unused.
  comments: number[];
  // closed issue numbers.
  closed: number[];
}

const REPOS: RepoSpec[] = [
  {
    name: "acme-api",
    issues: 10,
    comments: [0, 2, 1, 3, 2, 0, 1, 2, 1, 4, 2],
    closed: [2, 3, 6, 9],
  },
  {
    name: "acme-web",
    issues: 8,
    comments: [0, 1, 0, 2, 1, 3, 2, 1, 0],
    closed: [1, 4],
  },
  {
    name: "acme-cli",
    issues: 6,
    comments: [0, 1, 2, 0, 3, 1, 2],
    closed: [5],
  },
];

// ---- reset ----
rmSync(root, { recursive: true, force: true });

// ---- github tree ----
for (const repo of REPOS) {
  write(
    `github/repos/${repo.name}/README.md`,
    `# ${repo.name}\n\nSynthetic repository used by the omnifs agent benchmark. ` +
      `Contains ${repo.issues} issues under \`issues/<n>/\`.\n`,
  );

  for (let n = 1; n <= repo.issues; n += 1) {
    const state = repo.closed.includes(n) ? "closed" : "open";
    const bodyLines = [
      `# ${repo.name}#${n}: ${title(n)}`,
      "",
      `- state: ${state}`,
      `- label: ${label(n)}`,
      `- author: ${author(n)}`,
      "",
      `Describes issue ${n} in ${repo.name}. `,
    ];

    // Planted correlation fact: acme-api#3 tracks a Linear issue by key.
    if (repo.name === "acme-api" && n === 3) {
      bodyLines.push(
        "This work is tracked upstream in Linear as ENG-107; keep both in sync.",
      );
    }
    write(
      `github/repos/${repo.name}/issues/${n}/body.md`,
      bodyLines.join("\n"),
    );

    const cCount = repo.comments[n] ?? 0;
    for (let c = 1; c <= cCount; c += 1) {
      const lines = [
        `**${author(n + c)}** commented:`,
        "",
        `Comment ${c} on ${repo.name}#${n}.`,
      ];
      // Planted reconstruction fact: a config value proposed in acme-api#3 comments.
      if (repo.name === "acme-api" && n === 3 && c === 2) {
        lines.push(
          "After profiling the retries I propose we set max_retries=5 as the default.",
        );
      }
      write(
        `github/repos/${repo.name}/issues/${n}/comments/${c}.md`,
        lines.join("\n"),
      );
    }
  }
}

// ---- linear tree ----
write(
  "linear/teams.md",
  "# Linear teams\n\n- ENG: platform engineering (ENG-101..ENG-130)\n" +
    "- OPS: operations (OPS-201..OPS-215)\n- DES: design (DES-301..DES-320)\n",
);

interface TeamSpec {
  key: string;
  from: number;
  to: number;
}
const TEAMS: TeamSpec[] = [
  { key: "ENG", from: 101, to: 130 },
  { key: "OPS", from: 201, to: 215 },
  { key: "DES", from: 301, to: 320 },
];

const STATES = ["Backlog", "Todo", "In Progress", "In Review", "Done"];
const PRIORITIES = ["Low", "Medium", "High", "Urgent"];
const ASSIGNEES = [
  "Lena Ortiz",
  "Sam Boothe",
  "Nina Cole",
  "Omar Diaz",
  "Priya Rao",
];

for (const team of TEAMS) {
  for (let id = team.from; id <= team.to; id += 1) {
    const key = `${team.key}-${id}`;
    const assignee = ASSIGNEES[id % ASSIGNEES.length];
    const priority = PRIORITIES[id % PRIORITIES.length];
    const status = STATES[id % STATES.length];

    const issueLines = [
      `# ${key}`,
      "",
      `Title: ${title(id)}`,
      `Status: ${status}`,
      `Priority: ${priority}`,
      `Assignee: ${assignee}`,
      "",
      `Linear issue ${key} on the ${team.key} team.`,
    ];

    // Planted correlation facts.
    if (key === "ENG-107") {
      // corr-001 target: overwrite assignee/priority deterministically.
      issueLines[3] = "Status: In Progress";
      issueLines[4] = "Priority: High";
      issueLines[5] = "Assignee: Priya Rao";
      issueLines.push("Mirror of GitHub issue acme-api#3.");
    }
    if (key === "OPS-204") {
      issueLines.push(
        "Root-caused to the caching layer; see GitHub issue acme-web#7 for the fix.",
      );
    }
    write(`linear/issues/${key}/issue.md`, issueLines.join("\n"));

    // activity log
    const activityLines = [`# ${key} activity`, ""];
    if (key === "ENG-115") {
      // recon-001 target: a fixed transition sequence ending In Review -> Done.
      const seq = ["Backlog", "Todo", "In Progress", "In Review", "Done"];
      for (let i = 1; i < seq.length; i += 1) {
        activityLines.push(
          `- 2026-05-0${i}: status changed from ${seq[i - 1]} to ${seq[i]}`,
        );
      }
    } else {
      // deterministic short log for filler issues.
      const steps = (id % 3) + 1;
      for (let i = 0; i < steps; i += 1) {
        const a = STATES[(id + i) % STATES.length];
        const b = STATES[(id + i + 1) % STATES.length];
        activityLines.push(`- 2026-04-1${i}: status changed from ${a} to ${b}`);
      }
    }
    write(`linear/issues/${key}/activity.md`, activityLines.join("\n"));
  }
}

// ---- report ----
const closedAcmeApi = REPOS.find((r) => r.name === "acme-api")!.closed.length;
const acmeCliComments = REPOS.find((r) => r.name === "acme-cli")!.comments.reduce(
  (a, b) => a + b,
  0,
);
console.log(`fixture-data written: ${fileCount} files under ${root}`);
console.log(`  acme-api closed issues: ${closedAcmeApi}`);
console.log(`  acme-cli total comments: ${acmeCliComments}`);
