// Thin wrapper over the `gh` CLI for the changelog bot. Runs inside GitHub
// Actions where `gh` is authenticated via GITHUB_TOKEN. Kept narrow: read a PR's
// title/diff/author, read and seed reactions on the sticky comment, and upsert
// that comment.

import type { Repo } from "./repo";

type Comment = { id: number; body: string };
type Reaction = { content: string; login: string; createdAt: string };
export type UserIdentity = { login: string; name: string; email: string };

export class GitHub {
  constructor(private readonly repo: Repo) {}

  private gh(args: string[]) {
    return this.repo.$`gh ${args}`;
  }

  async prTitle(number: number): Promise<string> {
    return (await this.gh(["pr", "view", String(number), "--json", "title", "--jq", ".title"]).text()).trim();
  }

  async prDiff(number: number): Promise<string> {
    return await this.gh(["pr", "diff", String(number)]).text();
  }

  async prAuthorLogin(number: number): Promise<string> {
    return (
      await this.gh(["pr", "view", String(number), "--json", "author", "--jq", ".author.login"]).text()
    ).trim();
  }

  /**
   * Resolve a login to a git identity. Uses GitHub's no-reply email form
   * (`ID+login@users.noreply.github.com`) so the changelog commit can be authored
   * as the contributor without exposing a private address.
   */
  async userIdentity(login: string): Promise<UserIdentity> {
    const raw = await this.gh(["api", `users/${login}`, "--jq", "{id, name, login}"]).text();
    const u = JSON.parse(raw) as { id: number; name: string | null; login: string };
    return {
      login: u.login,
      name: u.name && u.name.length > 0 ? u.name : u.login,
      email: `${u.id}+${u.login}@users.noreply.github.com`,
    };
  }

  /** First issue comment carrying `marker`, or undefined. */
  async findComment(number: number, marker: string): Promise<Comment | undefined> {
    const raw = await this.gh([
      "api", "--paginate", `repos/{owner}/{repo}/issues/${number}/comments`,
      "--jq", ".[] | {id, body}",
    ]).text();
    for (const line of raw.split(/\r?\n/).filter(Boolean)) {
      const c = JSON.parse(line) as Comment;
      if (c.body.includes(marker)) return c;
    }
    return undefined;
  }

  /** Create the sticky comment, or edit it in place. Returns the comment id. */
  async upsertComment(number: number, marker: string, body: string): Promise<number> {
    const existing = await this.findComment(number, marker);
    if (existing) {
      await this.gh([
        "api", "-X", "PATCH", `repos/{owner}/{repo}/issues/comments/${existing.id}`,
        "-f", `body=${body}`,
      ]).quiet();
      return existing.id;
    }
    const raw = await this.gh([
      "api", `repos/{owner}/{repo}/issues/${number}/comments`,
      "-f", `body=${body}`, "--jq", ".id",
    ]).text();
    return Number(raw.trim());
  }

  /** Seed a reaction on a comment so the option is one-click for reviewers. */
  async addReaction(commentId: number, content: string): Promise<void> {
    await this.gh([
      "api", "-X", "POST", `repos/{owner}/{repo}/issues/comments/${commentId}/reactions`,
      "-f", `content=${content}`,
    ]).nothrow().quiet();
  }

  async commentReactions(commentId: number): Promise<Reaction[]> {
    const raw = await this.gh([
      "api", "--paginate", `repos/{owner}/{repo}/issues/comments/${commentId}/reactions`,
      "--jq", ".[] | {content, login: .user.login, createdAt: .created_at}",
    ]).text();
    return raw
      .split(/\r?\n/)
      .filter(Boolean)
      .map((line) => JSON.parse(line) as Reaction);
  }
}
