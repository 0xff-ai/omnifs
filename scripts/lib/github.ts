// Thin wrapper over the `gh` CLI for the release-note workflows. Runs inside
// GitHub Actions where `gh` is authenticated via GITHUB_TOKEN. Kept narrow:
// enumerate merged PRs, read/upsert the sticky release-note comment, and
// open/update the standing release PR.

import type { GitRepo } from "./git";
import type { Repo } from "./repo";

export type MergedPull = { number: number; subject: string };

export class GitHub {
  constructor(private readonly repo: Repo, private readonly git: GitRepo) {}

  private gh(args: string[]) {
    return this.repo.$`gh ${args}`;
  }

  /**
   * PR numbers merged since `sinceRef` (exclusive), newest last. Relies on
   * squash-merge commits carrying the `(#N)` suffix GitHub appends; matches the
   * repo's documented squash-merge release flow.
   */
  async mergedPullsSince(sinceRef: string | undefined): Promise<MergedPull[]> {
    const range = sinceRef ? `${sinceRef}..HEAD` : "HEAD";
    const log = await this.git.output(["log", "--first-parent", "--pretty=format:%s", range]);
    const seen = new Set<number>();
    const pulls: MergedPull[] = [];
    for (const subject of log.split(/\r?\n/).filter(Boolean).reverse()) {
      const m = subject.match(/\(#(\d+)\)\s*$/);
      if (!m) continue;
      const number = Number(m[1]);
      if (seen.has(number)) continue;
      seen.add(number);
      pulls.push({ number, subject: subject.replace(/\s*\(#\d+\)\s*$/, "") });
    }
    return pulls;
  }

  async prLabels(number: number): Promise<string[]> {
    const out = await this.gh(["pr", "view", String(number), "--json", "labels", "--jq", ".labels[].name"]).text();
    return out.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  }

  async prDiff(number: number): Promise<string> {
    return await this.gh(["pr", "diff", String(number)]).text();
  }

  async prTitle(number: number): Promise<string> {
    return (await this.gh(["pr", "view", String(number), "--json", "title", "--jq", ".title"]).text()).trim();
  }

  /** Body of the first issue comment carrying `marker`, or undefined. */
  async findComment(number: number, marker: string): Promise<{ id: number; body: string } | undefined> {
    const raw = await this.gh([
      "api", "--paginate", `repos/{owner}/{repo}/issues/${number}/comments`,
      "--jq", ".[] | {id, body}",
    ]).text();
    for (const line of raw.split(/\r?\n/).filter(Boolean)) {
      const c = JSON.parse(line) as { id: number; body: string };
      if (c.body.includes(marker)) return c;
    }
    return undefined;
  }

  /** Create the sticky comment, or edit it in place if it already exists. */
  async upsertComment(number: number, marker: string, body: string): Promise<void> {
    const existing = await this.findComment(number, marker);
    if (existing) {
      await this.gh([
        "api", "-X", "PATCH", `repos/{owner}/{repo}/issues/comments/${existing.id}`,
        "-f", `body=${body}`,
      ]).quiet();
      return;
    }
    await this.gh(["pr", "comment", String(number), "--body", body]).quiet();
  }

  async branchExistsOnRemote(branch: string): Promise<boolean> {
    const out = await this.repo.$`git ls-remote --heads origin ${branch}`.text();
    return out.trim().length > 0;
  }

  async openReleasePrNumber(branch: string): Promise<number | undefined> {
    const out = await this.gh([
      "pr", "list", "--head", branch, "--state", "open", "--json", "number", "--jq", ".[0].number",
    ]).text();
    const n = Number(out.trim());
    return Number.isFinite(n) && n > 0 ? n : undefined;
  }

  async createReleasePr(branch: string, title: string, body: string): Promise<void> {
    await this.gh([
      "pr", "create", "--base", "main", "--head", branch,
      "--title", title, "--body", body, "--label", "release",
    ]).quiet();
  }

  async updatePr(number: number, title: string, body: string): Promise<void> {
    await this.gh(["pr", "edit", String(number), "--title", title, "--body", body]).quiet();
  }
}
