import type { Repo } from "./repo";

export class GitRepo {
  constructor(private readonly repo: Repo) {}

  async output(args: string[]): Promise<string> {
    return await this.repo.$`git ${args}`.text();
  }

  async currentBranch(): Promise<string> {
    return (await this.output(["rev-parse", "--abbrev-ref", "HEAD"])).trim();
  }

  async ensureCleanTree(): Promise<void> {
    const status = (await this.output(["status", "--porcelain"])).trim();
    if (status.length > 0) {
      throw new Error("working tree is not clean; commit or stash changes before preparing a release");
    }
  }

  async latestSemverTag(): Promise<string | undefined> {
    return (await this.output(["tag", "-l", "v*.*.*", "--sort=-v:refname"])).split(/\r?\n/).find(Boolean);
  }

  async changedFiles(base: string, head: string): Promise<string[]> {
    return (await this.output(["diff", "--name-only", `${base}...${head}`]))
      .split(/\r?\n/)
      .filter(Boolean);
  }

  async show(spec: string): Promise<string> {
    return await this.output(["show", spec]);
  }

  /** Contents of a path at a ref, or undefined when the path/ref is absent. */
  async showOrUndefined(spec: string): Promise<string | undefined> {
    const result = await this.repo.$`git show ${spec}`.nothrow().quiet();
    return result.exitCode === 0 ? result.stdout.toString() : undefined;
  }

  async fetch(ref: string): Promise<void> {
    await this.repo.$`git fetch origin ${ref}`.nothrow().quiet();
  }

  async checkoutNewBranch(branch: string): Promise<void> {
    await this.repo.$`git checkout -b ${branch}`;
  }

  /** Create or reset `branch` to point at `start`, checking it out. */
  async resetBranchTo(branch: string, start: string): Promise<void> {
    await this.repo.$`git checkout -B ${branch} ${start}`;
  }

  async forcePushUpstream(branch: string): Promise<void> {
    await this.repo.$`git push -u --force-with-lease origin ${branch}`;
  }

  async addAll(): Promise<void> {
    await this.repo.$`git add -A`;
  }

  async commit(message: string): Promise<void> {
    await this.repo.$`git commit -m ${message}`;
  }

  async pushUpstream(branch: string): Promise<void> {
    await this.repo.$`git push -u origin ${branch}`;
  }
}
