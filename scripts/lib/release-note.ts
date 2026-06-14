// The per-PR changelog draft lives in a sticky bot comment, marked with MARKER.
// It carries three length options (short/medium/long) in human-readable form plus
// a hidden machine-readable data block, so the merge stage can recover the exact
// option text without re-calling the model. Selection is by emoji reaction:
//   short → 👍 (+1), medium → 🚀 (rocket), long → 👀 (eyes).
// At merge the chosen entry is appended to CHANGELOG.md's [Unreleased] section,
// grouped under its area heading. Appends are additive: existing lines are never
// rewritten, so hand-edits to CHANGELOG.md always survive.

import { areaById, areaIndex, resolveArea, type AreaId } from "./areas";
import type { ChangelogDraft } from "./llm";

export const MARKER = "<!-- omnifs-changelog -->";
const DATA_PREFIX = "<!-- omnifs-changelog-data:";
const DATA_SUFFIX = "-->";

export type AreaBullet = { area: AreaId; text: string };

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

// --- additive [Unreleased] fold, grouped by area --------------------------

/**
 * Additively fold new bullets into an existing [Unreleased] body. Existing lines
 * are preserved verbatim; new bullets are appended under their area's subsection
 * (created at its canonical position if absent). Exact-text duplicates within an
 * area are skipped, so recording the same entry twice is a no-op.
 */
export function appendBulletsToUnreleased(currentBody: string, newBullets: AreaBullet[]): string {
  const subs = parseSubsections(currentBody);
  for (const bullet of newBullets) {
    const text = `- ${bullet.text}`;
    const existing = subs.find((s) => s.areaId === bullet.area);
    if (existing) {
      if (!existing.bullets.some((b) => normalizeBullet(b) === normalizeBullet(text))) {
        existing.bullets.push(text);
      }
      continue;
    }
    insertSubsection(subs, {
      heading: `### ${areaById(bullet.area).heading}`,
      areaId: bullet.area,
      bullets: [text],
    });
  }
  return renderSubsections(subs);
}

type Subsection = { heading: string; areaId?: AreaId; bullets: string[] };

function parseSubsections(body: string): Subsection[] {
  const subs: Subsection[] = [];
  let current: Subsection | undefined;
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trimEnd();
    const heading = headingText(line);
    if (heading !== undefined) {
      current = { heading: line, areaId: resolveArea(heading)?.id, bullets: [] };
      subs.push(current);
      continue;
    }
    if (line.trim().length === 0) continue;
    if (!current) {
      // Loose content before any subsection: keep it as a headless section so
      // human-authored preamble in [Unreleased] is never dropped.
      current = { heading: "", bullets: [] };
      subs.push(current);
    }
    if (bulletBody(line) !== undefined) {
      current.bullets.push(line.trimStart());
    } else if (current.bullets.length > 0) {
      // Continuation of the previous bullet; keep attached verbatim.
      current.bullets[current.bullets.length - 1] += `\n${line}`;
    } else {
      current.bullets.push(line.trimStart());
    }
  }
  return subs.filter((s) => s.heading.length > 0 || s.bullets.length > 0);
}

function insertSubsection(subs: Subsection[], next: Subsection): void {
  const nextIdx = next.areaId ? areaIndex(next.areaId) : Number.MAX_SAFE_INTEGER;
  const at = subs.findIndex(
    (s) => s.areaId !== undefined && next.areaId !== undefined && areaIndex(s.areaId) > nextIdx,
  );
  if (at === -1) subs.push(next);
  else subs.splice(at, 0, next);
}

function renderSubsections(subs: Subsection[]): string {
  const blocks = subs
    .filter((s) => s.bullets.length > 0 || s.heading.length > 0)
    .map((s) => (s.heading.length > 0 ? [s.heading, ...s.bullets].join("\n") : s.bullets.join("\n")));
  return blocks.length === 0 ? "" : `${blocks.join("\n\n")}\n`;
}

function headingText(line: string): string | undefined {
  return line.startsWith("### ") ? line.slice(4).trim() : undefined;
}

function bulletBody(line: string): string | undefined {
  const m = line.match(/^\s*-\s+(.*)$/);
  return m ? m[1]!.trim() : undefined;
}

function normalizeBullet(line: string): string {
  return (bulletBody(line) ?? line).trim().toLowerCase().replace(/\s+/g, " ");
}
