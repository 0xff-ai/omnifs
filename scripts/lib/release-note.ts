// The per-PR changelog draft lives in a sticky bot comment, marked with MARKER.
// It carries three length options (short/medium/long) in human-readable form plus
// a hidden machine-readable data block, so the merge stage can recover the exact
// option text without re-calling the model. Selection is by emoji reaction:
//   short → 👍 (+1), medium → 🚀 (rocket), long → 👀 (eyes).
// At merge the chosen entry is appended to CHANGELOG.md's [Unreleased] section,
// grouped under its area heading. Appends are additive: existing lines are never
// rewritten, so hand-edits to CHANGELOG.md always survive.

import { areaById } from "./areas";
import type { ChangelogDraft } from "./llm";

export const MARKER = "<!-- omnifs-changelog -->";
const DATA_PREFIX = "<!-- omnifs-changelog-data:";
const DATA_SUFFIX = "-->";

/** Reaction content (GitHub's fixed set) that selects each length option. */
export const OPTION_REACTIONS = { short: "+1", medium: "rocket", long: "eyes" } as const;
export type OptionLength = keyof typeof OPTION_REACTIONS;

const REACTION_TO_LENGTH = new Map<string, OptionLength>(
  Object.entries(OPTION_REACTIONS).map(([length, content]) => [content, length as OptionLength]),
);

/** The option length a reaction selects, or undefined if it is not a selector. */
export function lengthForReaction(content: string): OptionLength | undefined {
  return REACTION_TO_LENGTH.get(content);
}

/** Render the sticky comment body: human-readable options plus a hidden data block. */
export function renderOptionsComment(draft: ChangelogDraft): string {
  // base64 so option text containing "-->" cannot truncate the data block early.
  const data = `${DATA_PREFIX} ${Buffer.from(JSON.stringify(draft), "utf8").toString("base64")} ${DATA_SUFFIX}`;
  if (draft.skip) {
    return [
      MARKER,
      "",
      "_No changelog entry: this PR looks like it has no user-facing change. Edit `CHANGELOG.md` directly if that is wrong._",
      data,
    ].join("\n");
  }
  return [
    MARKER,
    "",
    `**Changelog entry** under _${areaById(draft.area).heading}_. React to pick the length that lands in the next release; with no reaction the medium default is used, and a maintainer's pick always wins.`,
    "",
    `- 👍 short: ${draft.short}`,
    `- 🚀 medium: ${draft.medium}`,
    `- 👀 long: ${draft.long}`,
    "",
    "_This never overwrites hand-edits to `CHANGELOG.md`._",
    data,
  ].join("\n");
}

/** Recover the drafted options from a sticky comment body, or undefined. */
export function parseOptionsComment(body: string): ChangelogDraft | undefined {
  if (!body.includes(MARKER)) return undefined;
  const start = body.indexOf(DATA_PREFIX);
  if (start === -1) return undefined;
  const end = body.indexOf(DATA_SUFFIX, start + DATA_PREFIX.length);
  if (end === -1) return undefined;
  try {
    const json = Buffer.from(body.slice(start + DATA_PREFIX.length, end).trim(), "base64").toString("utf8");
    return JSON.parse(json) as ChangelogDraft;
  } catch {
    return undefined;
  }
}

