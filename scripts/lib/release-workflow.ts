import { semver } from "bun";
import { createInterface } from "node:readline/promises";
import { stdin, stderr } from "node:process";
import {
  finalizeUnreleased,
  parseChangelog,
  sectionForVersion,
  unreleasedHasContent,
  validateReleaseChangelog,
  type Changelog,
} from "./changelog";
import { GitRepo } from "./git";
import { NpmWorkspace } from "./npm-workspace";
import { Repo, printErrorsAndExit } from "./repo";

export type ShipPlan = {
  should_ship: boolean;
  version: string;
  tag: string;
  release_notes?: string;
};

export class ReleaseWorkflow {
  private readonly git: GitRepo;
  private readonly npm: NpmWorkspace;

  constructor(private readonly repo: Repo) {
    this.git = new GitRepo(repo);
    this.npm = new NpmWorkspace(repo);
  }

  /** Validate a release PR: finalized changelog section, version, npm sync. */
  async releaseCheck(): Promise<void> {
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

async function promptTargetVersion(current: string): Promise<string> {
  const suggested = suggestNextPatch(current);
  const rl = createInterface({ input: stdin, output: stderr });
  const input = (await rl.question(`Current workspace version: ${current}\nSuggested patch release: ${suggested}\nTarget version [${suggested}]: `)).trim();
  rl.close();
  return input || suggested;
}

/** Suggest the next patch version, dropping any prerelease suffix. */
function suggestNextPatch(current: string): string {
  const core = current.split("-")[0] ?? current;
  const [major = "0", minor = "0", patch = "0"] = core.split(".");
  return `${major}.${minor}.${Number(patch) + 1}`;
}
