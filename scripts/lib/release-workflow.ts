import { semver } from "bun";
import { createInterface } from "node:readline/promises";
import { stdin, stderr } from "node:process";
import { AREAS } from "./areas";
import {
  finalizeUnreleased,
  parseChangelog,
  sectionForVersion,
  unreleasedHasContent,
  validateReleaseChangelog,
  withReleaseSection,
  type Changelog,
} from "./changelog";
import { GitRepo } from "./git";
import { GitHub } from "./github";
import { llmComplete } from "./llm";
import { NpmWorkspace } from "./npm-workspace";
import {
  MARKER,
  appendBulletsToUnreleased,
  parseReleaseNoteComment,
  renderReleaseNoteComment,
  type AreaBullet,
  type ReleaseNote,
} from "./release-note";
import { Repo, printErrorsAndExit } from "./repo";
import { bump, bumpPatch, formatVersion, parseVersion, type BumpLevel } from "./semver";

const MANIFEST_PATH = ".release-notes-manifest.json";

type ReleaseManifest = { target: string; date: string; folded: number[] };

export type ShipPlan = {
  should_ship: boolean;
  version: string;
  tag: string;
  release_notes?: string;
};

export class ReleaseWorkflow {
  private readonly git: GitRepo;
  private readonly github: GitHub;
  private readonly npm: NpmWorkspace;

  constructor(private readonly repo: Repo) {
    this.git = new GitRepo(repo);
    this.github = new GitHub(repo, this.git);
    this.npm = new NpmWorkspace(repo);
  }

  async releaseNotesPrompt(): Promise<string> {
    const tag = await this.git.latestSemverTag();
    const range = tag ? `${tag}..HEAD` : "HEAD";
    return `# Release notes prompt

Write a Keep a Changelog \`## [Unreleased]\` section for omnifs from the commit range below.
Inspect the repo with git (log, diff, show) as needed. Use end-user language, merge related
changes, and omit internal-only refactors unless they affect users. Use \`### Added\`,
\`### Changed\`, and \`### Fixed\` where appropriate.

Return only the markdown body for \`[Unreleased]\` (subsection headings and bullets). Do not
include the \`## [Unreleased]\` heading itself.

## Commit range

${range}
`;
  }

  // --- per-PR release note (editable sticky comment) ----------------------

  /**
   * Draft an area-tagged release note for a PR and post it as the sticky
   * comment. Leaves an existing comment untouched unless `force`, so human
   * edits to the note survive later pushes.
   */
  async draftPrReleaseNote(prNumber: number, force = false): Promise<void> {
    const labels = await this.github.prLabels(prNumber);
    const existing = await this.github.findComment(prNumber, MARKER);
    if (existing && !force) {
      console.log(`#${prNumber} already has a release-note comment; leaving it intact`);
      return;
    }
    if (labels.includes("no-changelog")) {
      await this.github.upsertComment(prNumber, MARKER, renderReleaseNoteComment(skipNote()));
      console.log(`#${prNumber} labeled no-changelog; posted opt-out note`);
      return;
    }

    const title = await this.github.prTitle(prNumber);
    const diff = await this.github.prDiff(prNumber);
    const draft = await llmComplete({ system: draftSystemPrompt(), user: draftUserPrompt(title, diff) });
    if (draft.trim().toUpperCase() === "NONE") {
      await this.github.upsertComment(prNumber, MARKER, renderReleaseNoteComment(skipNote()));
      console.log(`#${prNumber}: model found no user-facing change; posted opt-out note`);
      return;
    }

    const parsed = parseReleaseNoteComment(`${MARKER}\n${draft}`);
    const bullets = parsed?.bullets ?? [];
    await this.github.upsertComment(
      prNumber,
      MARKER,
      renderReleaseNoteComment({ skip: bullets.length === 0, bullets, unknownHeadings: [] }),
    );
    console.log(`#${prNumber}: posted ${bullets.length} release-note bullet(s)`);
  }

  /** CI gate: a PR must carry a non-empty release note or the no-changelog label. */
  async checkPrReleaseNote(prNumber: number): Promise<void> {
    const labels = await this.github.prLabels(prNumber);
    if (labels.includes("no-changelog")) {
      console.log(`#${prNumber}: no-changelog label present; release note not required`);
      return;
    }
    const comment = await this.github.findComment(prNumber, MARKER);
    if (!comment) {
      printErrorsAndExit("release note check", [
        `PR #${prNumber} has no release-note comment. The drafter posts one on open; add the no-changelog label to exempt chore-only PRs.`,
      ]);
    }
    const parsed = parseReleaseNoteComment(comment.body)!;
    if (parsed.skip) {
      console.log(`#${prNumber}: release note marked skip`);
      return;
    }
    if (parsed.bullets.length === 0) {
      printErrorsAndExit("release note check", [
        `PR #${prNumber} release-note comment has no bullets. Add at least one under an area heading, or apply the no-changelog label.`,
      ]);
    }
    console.log(`release note check passed for #${prNumber}`);
  }

  // --- standing release PR (release-please shape, additive) ----------------

  /**
   * Maintain the standing release PR: fold release notes from PRs merged since
   * the last tag into a `release/vX.Y.Z` branch, additively. Existing CHANGELOG
   * lines are preserved (edit-safe); only newly merged PRs are appended. Merging
   * the resulting PR ships through the existing release pipeline.
   */
  async maintainReleasePr(): Promise<void> {
    await this.git.fetch("main");
    const tag = await this.git.latestSemverTag();
    const pulls = await this.github.mergedPullsSince(tag);
    if (pulls.length === 0) {
      console.log("no merged PRs since the last release; nothing to maintain");
      return;
    }

    const labelSets: string[][] = [];
    const notes: { number: number; bullets: AreaBullet[] }[] = [];
    const unnoted: number[] = [];
    for (const pull of pulls) {
      const labels = await this.github.prLabels(pull.number);
      labelSets.push(labels);
      if (labels.includes("no-changelog")) continue;
      const comment = await this.github.findComment(pull.number, MARKER);
      const parsed = comment ? parseReleaseNoteComment(comment.body) : null;
      if (!parsed || (!parsed.skip && parsed.bullets.length === 0)) {
        unnoted.push(pull.number);
        continue;
      }
      if (parsed.skip) continue;
      notes.push({ number: pull.number, bullets: parsed.bullets });
    }

    if (notes.length === 0) {
      console.log("no release-note bullets to fold yet");
      return;
    }

    const baseVersion = tag ? tag.replace(/^v/, "") : await this.repo.workspaceVersion();
    const target = formatVersion(bump(parseVersion(baseVersion), bumpLevelFromLabels(labelSets)));
    const branch = `release/v${target}`;

    await this.git.fetch(branch);
    const branchExists = await this.github.branchExistsOnRemote(branch);
    const manifest = branchExists ? await this.readManifest(branch) : undefined;
    const date = manifest?.date ?? todayUtc();

    let priorBody = "";
    if (branchExists) {
      const raw = await this.git.showOrUndefined(`origin/${branch}:CHANGELOG.md`);
      if (raw) priorBody = sectionForVersion(parseChangelog(raw), target)?.body ?? "";
    } else {
      // First fold for this version: migrate any hand-written [Unreleased] notes
      // on main into the release section so existing content is not dropped.
      priorBody = (await this.readChangelog()).unreleasedBody;
    }
    const folded = new Set(manifest?.folded ?? []);
    const newBullets = notes.filter((note) => !folded.has(note.number)).flatMap((note) => note.bullets);
    if (branchExists && newBullets.length === 0) {
      console.log(`release PR for v${target} is already current`);
      return;
    }
    const sectionBody = appendBulletsToUnreleased(priorBody, newBullets);
    for (const note of notes) folded.add(note.number);
    const foldedList = Array.from(folded).sort((a, b) => a - b);

    // Rebuild the branch cleanly on main, then apply the accumulated section and bumps.
    await this.git.resetBranchTo(branch, "origin/main");
    const mainLog = await this.readChangelog();
    const finalized = withReleaseSection(mainLog, target, sectionBody, date);
    const errors = validateReleaseChangelog(finalized, target);
    if (errors.length > 0) {
      printErrorsAndExit("release PR maintenance", errors);
    }
    await Bun.write(this.repo.path("CHANGELOG.md"), finalized.raw);

    await this.bumpWorkspaceVersion(target);
    await this.repo.$`cargo update --workspace`;
    await this.npm.sync(target);
    const syncErrors = await this.npm.validateSynced(target);
    if (syncErrors.length > 0) {
      printErrorsAndExit("release PR maintenance", syncErrors);
    }
    await this.npm.validate();

    await this.writeManifest({ target, date, folded: foldedList });

    await this.git.addAll();
    await this.git.commit(`release: prepare v${target}`);
    await this.git.forcePushUpstream(branch);

    const title = `release: v${target}`;
    const body = releasePrBody(target, foldedList, unnoted);
    const open = await this.github.openReleasePrNumber(branch);
    if (open) await this.github.updatePr(open, title, body);
    else await this.github.createReleasePr(branch, title, body);
    console.log(`maintained release PR for v${target} (${foldedList.length} PR notes folded)`);
  }

  private async readManifest(branch: string): Promise<ReleaseManifest | undefined> {
    const raw = await this.git.showOrUndefined(`origin/${branch}:${MANIFEST_PATH}`);
    if (!raw) return undefined;
    try {
      return JSON.parse(raw) as ReleaseManifest;
    } catch {
      return undefined;
    }
  }

  private async writeManifest(manifest: ReleaseManifest): Promise<void> {
    await Bun.write(this.repo.path(MANIFEST_PATH), `${JSON.stringify(manifest, null, 2)}\n`);
  }

  async releaseCheck(base: string, head: string): Promise<void> {
    const branch = process.env.GITHUB_HEAD_REF || await this.git.currentBranch();
    if (branch.startsWith("release/")) {
      await this.checkReleasePr();
    } else {
      await this.checkChangelogPr(base, head);
    }
  }

  async shipPlan(): Promise<ShipPlan> {
    const version = await this.repo.workspaceVersion();
    const tag = await this.git.latestSemverTag();
    const shouldShip = !tag || semver.order(version, tag.replace(/^v/, "")) > 0;
    if (!shouldShip) {
      return { should_ship: false, version, tag: `v${version}` };
    }

    const log = await this.readChangelog();
    const errors = [
      ...validateReleaseChangelog(log, version),
      ...await this.npm.validateSynced(version),
    ];
    if (errors.length > 0) {
      printErrorsAndExit("release plan", errors);
    }
    await this.npm.validate();
    const releaseNotes = sectionForVersion(log, version)?.body.trim();
    if (!releaseNotes) {
      throw new Error("release notes missing");
    }
    return { should_ship: true, version, tag: `v${version}`, release_notes: releaseNotes };
  }

  async releaseCut(requestedVersion: string | undefined, push: boolean): Promise<void> {
    await this.git.ensureCleanTree();
    const branchName = await this.git.currentBranch();
    if (branchName !== "main") {
      throw new Error(`expected to be on branch main, but on ${branchName}`);
    }

    const current = await this.repo.workspaceVersion();
    const target = requestedVersion ?? await promptTargetVersion(current);
    if (semver.order(target, current) <= 0) {
      throw new Error(`target version ${target} must be greater than current workspace version ${current}`);
    }

    if (!unreleasedHasContent(await this.readChangelog())) {
      throw new Error("CHANGELOG.md [Unreleased] is empty; add release notes on main before cutting a release");
    }

    const branch = `release/v${target}`;
    await this.git.checkoutNewBranch(branch);

    await this.bumpWorkspaceVersion(target);
    await this.repo.$`cargo update --workspace`;
    await this.npm.sync(target);

    const finalized = finalizeUnreleased(await this.readChangelog(), target);
    const errors = validateReleaseChangelog(finalized, target);
    if (errors.length > 0) {
      printErrorsAndExit("release cut", errors);
    }
    await Bun.write(this.repo.path("CHANGELOG.md"), finalized.raw);

    const syncErrors = await this.npm.validateSynced(target);
    if (syncErrors.length > 0) {
      printErrorsAndExit("release cut", syncErrors);
    }
    await this.npm.validate();

    await this.git.addAll();
    await this.git.commit(`release: v${target}`);
    if (push) {
      await this.publishReleasePr(branch, target);
    }

    console.log(`prepared release v${target} on branch ${branch}`);
  }

  private async readChangelog(): Promise<Changelog> {
    return parseChangelog(await Bun.file(this.repo.path("CHANGELOG.md")).text());
  }

  private async checkChangelogPr(base: string, head: string): Promise<void> {
    const changed = await this.git.changedFiles(base, head);
    if (!changed.includes("CHANGELOG.md")) {
      throw new Error("PR must update CHANGELOG.md under ## [Unreleased]; add the no-changelog label to exempt chore-only PRs");
    }

    const baseLog = parseChangelog(await this.git.show(`${base}:CHANGELOG.md`));
    const headLog = await this.readChangelog();
    if (headLog.unreleasedBody.trim() === baseLog.unreleasedBody.trim()) {
      throw new Error("CHANGELOG.md [Unreleased] was not updated");
    }
    if (!unreleasedHasContent(headLog)) {
      throw new Error("CHANGELOG.md [Unreleased] must contain release notes");
    }
    console.log("changelog PR check passed");
  }

  private async checkReleasePr(): Promise<void> {
    const version = await this.repo.workspaceVersion();
    const errors = [
      ...validateReleaseChangelog(await this.readChangelog(), version),
      ...await this.npm.validateSynced(version),
    ];
    if (errors.length > 0) {
      printErrorsAndExit("release PR check", errors);
    }
    await this.npm.validate();
    console.log(`release PR check passed for version ${version}`);
  }

  private async bumpWorkspaceVersion(version: string): Promise<void> {
    const result = await this.repo.$`cargo set-version ${version}`.nothrow().quiet();
    if (result.exitCode === 0) return;

    const stderr = result.stderr.toString().trim();
    if (stderr.includes("no such subcommand") || stderr.includes("set-version")) {
      throw new Error("cargo set-version unavailable; install cargo-edit with `cargo install cargo-edit`");
    }
    throw new Error(`cargo set-version failed${stderr ? `: ${stderr}` : ""}`);
  }

  private async publishReleasePr(branch: string, target: string): Promise<void> {
    const body = `Prepare omnifs v${target}.

- Finalizes CHANGELOG.md
- Bumps workspace, lockfile, and npm versions

Merging this PR triggers CI, then the Release workflow after green CI.`;
    await this.git.pushUpstream(branch);
    await this.repo.$`gh pr create --base main --head ${branch} --title ${`release: v${target}`} --body ${body} --label release`;
  }
}

function bumpLevelFromLabels(labelSets: string[][]): BumpLevel {
  const labels = labelSets.flat();
  if (labels.includes("release:major")) return "major";
  if (labels.includes("release:minor")) return "minor";
  return "patch";
}

function skipNote(): ReleaseNote {
  return { skip: true, bullets: [], unknownHeadings: [] };
}

function draftSystemPrompt(): string {
  const areas = AREAS.map((area) => `- ${area.heading}`).join("\n");
  return `You write end-user release notes for omnifs, a projected filesystem that mirrors external services into local paths.

From the PR title and diff, write concise, user-facing changelog bullets. Group them under these exact area headings (omit any area with no relevant change):
${areas}

Rules:
- Output ONLY markdown: \`### <Area heading>\` lines followed by \`- \` bullets. No preamble, no closing remarks.
- Plain end-user language describing observable behavior, not implementation. Merge related changes into one bullet.
- Omit internal-only refactors, test-only changes, and CI/chore edits.
- If the PR has no user-facing change, output exactly: NONE`;
}

function draftUserPrompt(title: string, diff: string): string {
  const MAX_DIFF = 60_000;
  const clipped = diff.length > MAX_DIFF ? `${diff.slice(0, MAX_DIFF)}\n... [diff truncated]` : diff;
  return `PR title: ${title}\n\nUnified diff:\n${clipped}`;
}

function releasePrBody(target: string, folded: number[], unnoted: number[]): string {
  const lines = [
    `Standing release PR for omnifs v${target}. Updated automatically as PRs merge into main.`,
    "",
    `Release notes accumulate in CHANGELOG.md under \`## [${target}]\`. Edit that section directly in this PR; the maintainer only appends bullets for newly merged PRs and never rewrites existing lines.`,
    "",
    `Folded PRs: ${folded.length ? folded.map((n) => `#${n}`).join(", ") : "none"}`,
  ];
  if (unnoted.length > 0) {
    lines.push(
      "",
      `> Merged PRs without a release note (excluded): ${unnoted.map((n) => `#${n}`).join(", ")}. Add a note comment or the \`no-changelog\` label.`,
    );
  }
  lines.push("", "Merging this PR triggers CI, then the Release workflow after green CI.");
  return lines.join("\n");
}

function todayUtc(): string {
  const now = new Date();
  return `${now.getUTCFullYear()}-${String(now.getUTCMonth() + 1).padStart(2, "0")}-${String(now.getUTCDate()).padStart(2, "0")}`;
}

async function promptTargetVersion(current: string): Promise<string> {
  const suggested = formatVersion(bumpPatch(parseVersion(current)));
  const rl = createInterface({ input: stdin, output: stderr });
  const input = (await rl.question(`Current workspace version: ${current}\nSuggested patch release: ${suggested}\nTarget version [${suggested}]: `)).trim();
  rl.close();
  return input || suggested;
}
