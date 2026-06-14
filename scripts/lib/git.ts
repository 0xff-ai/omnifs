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

  async checkoutNewBranch(branch: string): Promise<void> {
    await this.repo.$`git checkout -b ${branch}`;
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
