// The changelog bot has two stages, both LLM-drafted and area-grouped:
//
//   draft(pr):  on a feature PR, draft one entry in three lengths and post it as
//               a sticky comment with the three selector reactions seeded. No CI
//               gate; contributors are never obliged to write anything.
//
//   record():   on each push to main, append the chosen entry for the merged PR
//               to CHANGELOG.md's [Unreleased], grouped under its area, and commit
//               it authored as the PR author. Selection is by reaction:
//               maintainer's pick wins, else the author's, else the medium default.
//
// Direct pushes to main (no PR) are drafted from the commit diff and recorded at
// the medium length, authored as the pusher.

import { appendBulletsToUnreleased, parseChangelog, withUnreleased } from "./changelog";
import { GitHub, type UserIdentity } from "./github";
import { draftChangelogOptions, type ChangelogDraft } from "./llm";
import {
  MARKER,
  OPTION_REACTIONS,
  lengthForReaction,
  parseOptionsComment,
  renderOptionsComment,
  type OptionLength,
} from "./release-note";
import type { Repo } from "./repo";

const CHANGELOG = "CHANGELOG.md";
// Loop guard: the record job ignores commits with this subject prefix, which the
// bot's own changelog commits carry.
const COMMIT_PREFIX = "docs(changelog):";
// Release-cut commits finalize [Unreleased]; recording into the freshly-emptied
// section would append a spurious entry, so skip them deterministically.
const RELEASE_SUBJECT = /^release: v\d/;
const DEFAULT_MAINTAINERS = ["raulk"];
const DEFAULT_BOT_LOGIN = "github-actions[bot]";

export class ChangelogBot {
  private readonly github: GitHub;

  constructor(private readonly repo: Repo) {
    this.github = new GitHub(repo);
  }

  /** PR stage: draft three length options and post them as a sticky comment. */
  async draft(prNumber: number): Promise<void> {
    if (!process.env.OPENCODE_ZEN_API_KEY) {
      // Fork PRs cannot read the secret; skip cleanly and let the record stage
      // draft from the merged diff instead of failing the PR's checks.
      console.log(`#${prNumber}: no OPENCODE_ZEN_API_KEY (likely a fork PR); will draft at merge`);
      return;
    }
    if (await this.github.findComment(prNumber, MARKER)) {
      console.log(`#${prNumber}: changelog comment exists; leaving it intact`);
      return;
    }
    const title = await this.github.prTitle(prNumber);
    const diff = await this.github.prDiff(prNumber);
    const draft = await draftChangelogOptions(title, diff);
    const commentId = await this.github.upsertComment(prNumber, MARKER, renderOptionsComment(draft));
    if (!draft.skip) {
      for (const content of Object.values(OPTION_REACTIONS)) {
        await this.github.addReaction(commentId, content);
      }
    }
    console.log(`#${prNumber}: ${draft.skip ? "no user-facing change; posted opt-out" : "posted 3 length options"}`);
  }

  /** Merge stage: record the chosen entry for HEAD into CHANGELOG [Unreleased]. */
  async record(): Promise<void> {
    const subject = await this.headSubject();
    if (subject.startsWith(COMMIT_PREFIX)) {
      console.log("HEAD is a changelog commit; nothing to record");
      return;
    }
    if (RELEASE_SUBJECT.test(subject)) {
      console.log("HEAD is a release commit; nothing to record");
      return;
    }
    const prNumber = parsePrNumber(subject);
    if (prNumber !== undefined) {
      await this.recordFromPr(prNumber);
    } else {
      await this.recordFromCommit(subject);
    }
  }

  private async recordFromPr(prNumber: number): Promise<void> {
    const authorLogin = await this.github.prAuthorLogin(prNumber);
    const identity = await this.github.userIdentity(authorLogin);
    const comment = await this.github.findComment(prNumber, MARKER);
    let draft = comment ? parseOptionsComment(comment.body) : undefined;
    let length: OptionLength = "medium";

    if (draft && comment && !draft.skip) {
      length = await this.resolveLength(comment.id, authorLogin);
    }
    if (!draft) {
      // The bot never drafted (e.g. a fork PR with no secret access): draft now
      // from the merged diff, at the default length.
      const title = await this.github.prTitle(prNumber);
      const diff = await this.github.prDiff(prNumber);
      draft = await draftChangelogOptions(title, diff);
    }
    await this.commitEntry(draft, length, identity, `#${prNumber}`);
  }

  private async recordFromCommit(subject: string): Promise<void> {
    const diff = await this.repo.$`git show HEAD --format= --no-color`.text();
    const name = (await this.repo.$`git log -1 --format=%an`.text()).trim();
    const email = (await this.repo.$`git log -1 --format=%ae`.text()).trim();
    const draft = await draftChangelogOptions(subject, diff);
    await this.commitEntry(draft, "medium", { login: name, name, email }, subject);
  }

  /** Pick the option length: maintainer reaction wins, then author, then medium. */
  private async resolveLength(commentId: number, authorLogin: string): Promise<OptionLength> {
    const picks = (await this.github.commentReactions(commentId))
      .filter((r) => r.login !== this.botLogin)
      .map((r) => ({ login: r.login, createdAt: r.createdAt, length: lengthForReaction(r.content) }))
      .filter((r): r is { login: string; createdAt: string; length: OptionLength } => r.length !== undefined)
      .sort((a, b) => b.createdAt.localeCompare(a.createdAt));

    const maintainerPick = picks.find((p) => this.maintainers.includes(p.login));
    if (maintainerPick) return maintainerPick.length;
    const authorPick = picks.find((p) => p.login === authorLogin);
    if (authorPick) return authorPick.length;
    return "medium";
  }

  private async commitEntry(
    draft: ChangelogDraft,
    length: OptionLength,
    identity: UserIdentity,
    ref: string,
  ): Promise<void> {
    if (draft.skip) {
      console.log(`${ref}: no user-facing change; nothing recorded`);
      return;
    }
    const text = draft[length].trim();
    if (text.length === 0) {
      console.log(`${ref}: empty ${length} entry; nothing recorded`);
      return;
    }
    const log = parseChangelog(await Bun.file(this.repo.path(CHANGELOG)).text());
    const body = appendBulletsToUnreleased(log.unreleasedBody, [{ area: draft.area, text }]);
    if (body === log.unreleasedBody) {
      console.log(`${ref}: entry already present; no-op`);
      return;
    }
    await Bun.write(this.repo.path(CHANGELOG), withUnreleased(log, body).raw);
    await this.repo.$`git add ${CHANGELOG}`;
    await this.repo.$`git commit -m ${`${COMMIT_PREFIX} record ${ref}`} --author=${`${identity.name} <${identity.email}>`}`;
    // Rebase before pushing so a merge that landed on main during this run does
    // not turn the push into a rejected non-fast-forward.
    await this.repo.$`git pull --rebase origin main`;
    await this.repo.$`git push origin HEAD:main`;
    console.log(`${ref}: recorded ${length} entry under ${draft.area}`);
  }

  private async headSubject(): Promise<string> {
    return (await this.repo.$`git log -1 --format=%s`.text()).trim();
  }

  private get maintainers(): string[] {
    const env = process.env.OMNIFS_CHANGELOG_MAINTAINERS;
    return env ? env.split(",").map((s) => s.trim()).filter(Boolean) : DEFAULT_MAINTAINERS;
  }

  private get botLogin(): string {
    return process.env.OMNIFS_BOT_LOGIN || DEFAULT_BOT_LOGIN;
  }
}

/** PR number from a squash-merge subject's trailing `(#N)`, or undefined. */
function parsePrNumber(subject: string): number | undefined {
  const m = subject.match(/\(#(\d+)\)\s*$/);
  return m ? Number(m[1]) : undefined;
}
