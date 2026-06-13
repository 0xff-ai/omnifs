// The per-PR release note lives in an editable sticky bot comment, marked with
// MARKER so the drafter can find and update it. Collaborators edit the bullets
// in place; that comment body is the canonical capture. The standing release PR
// later folds these notes, additively, into CHANGELOG.md's [Unreleased] section.
//
// Two kinds of edit must survive untouched:
//   1. a human editing a bullet's wording in the PR comment, and
//   2. a human editing CHANGELOG.md directly in the release PR.
// Both are honored by only ever appending: existing lines are never rewritten.

import { AREAS, type AreaId, areaById, areaIndex, resolveArea } from "./areas";

export const MARKER = "<!-- omnifs-release-notes -->";
const SKIP_MARKER = "<!-- omnifs-release-notes:skip -->";

export type AreaBullet = { area: AreaId; text: string };

export type ReleaseNote = {
  /** Opt-out: this PR contributes nothing to the changelog (chore/internal). */
  skip: boolean;
  bullets: AreaBullet[];
  /** Area-ish headings in the comment that did not resolve to a known area. */
  unknownHeadings: string[];
};

/**
 * Parse a PR comment body. Returns null when the comment is not one of our
 * sticky release-note comments (no MARKER). Bullets under an unrecognized or
 * absent heading are attributed to the trailing catch-all area so nothing is
 * silently dropped.
 */
export function parseReleaseNoteComment(body: string): ReleaseNote | null {
  if (!body.includes(MARKER)) return null;
  const skip = body.includes(SKIP_MARKER);
  const bullets: AreaBullet[] = [];
  const unknownHeadings: string[] = [];
  let current: AreaId | undefined;

  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trimEnd();
    const heading = headingText(line);
    if (heading !== undefined) {
      const area = resolveArea(heading);
      if (area) {
        current = area.id;
      } else {
        unknownHeadings.push(heading);
        current = AREAS[AREAS.length - 1]!.id;
      }
      continue;
    }
    const bulletText = bulletBody(line);
    if (bulletText !== undefined && bulletText.length > 0) {
      bullets.push({ area: current ?? AREAS[AREAS.length - 1]!.id, text: bulletText });
    }
  }

  return { skip, bullets, unknownHeadings };
}

/** Render the sticky comment body for a drafted note (or an opt-out stub). */
export function renderReleaseNoteComment(note: ReleaseNote): string {
  if (note.skip) {
    return [
      MARKER,
      SKIP_MARKER,
      "",
      "_No release note for this PR (chore/internal)._",
      "",
      "Edit this comment and add bullets under an area heading to contribute a note.",
    ].join("\n");
  }
  return [
    MARKER,
    "",
    "**Release note** — edit the bullets below; they are collected into the next release's changelog.",
    "Remove all bullets (or add the `no-changelog` label) to skip this PR.",
    "",
    renderBulletSubsections(note.bullets),
  ].join("\n");
}

/**
 * Additively fold new bullets into an existing [Unreleased] body. Existing
 * lines are preserved verbatim; new bullets are appended under their area's
 * subsection (created at its canonical position if absent). Exact-text
 * duplicates within an area are skipped, so re-folding the same PR is a no-op.
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

/** Build a fresh [Unreleased] body from a bullet set, grouped in area order. */
export function renderBulletSubsections(bullets: AreaBullet[]): string {
  return appendBulletsToUnreleased("", bullets);
}

// --- internal: structured [Unreleased] body model -------------------------

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
