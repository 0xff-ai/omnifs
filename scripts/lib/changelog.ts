import { areaById, areaIndex, resolveArea, type AreaId } from "./areas";

export type ChangelogSection = {
  heading: string;
  body: string;
};

export type AreaBullet = { area: AreaId; text: string };

export type Changelog = {
  raw: string;
  preamble: string;
  unreleasedBody: string;
  sections: ChangelogSection[];
};

export function parseChangelog(raw: string): Changelog {
  const preamble: string[] = [];
  const unreleasedBody: string[] = [];
  const sections: ChangelogSection[] = [];
  let currentHeading: string | undefined;
  let currentBody: string[] = [];
  let seenUnreleased = false;

  for (const line of raw.split(/\r?\n/)) {
    if (line.startsWith("## [")) {
      if (currentHeading && !currentHeading.startsWith("## [Unreleased]")) {
        sections.push({ heading: currentHeading, body: linesToBody(currentBody) });
        currentBody = [];
      }

      currentHeading = line;
      if (line.startsWith("## [Unreleased]")) {
        if (seenUnreleased) {
          throw new Error("CHANGELOG.md must contain only one ## [Unreleased] section");
        }
        seenUnreleased = true;
      }
      continue;
    }

    if (!currentHeading) {
      preamble.push(line);
    } else if (currentHeading.startsWith("## [Unreleased]")) {
      unreleasedBody.push(line);
    } else {
      currentBody.push(line);
    }
  }

  if (currentHeading && !currentHeading.startsWith("## [Unreleased]")) {
    sections.push({ heading: currentHeading, body: linesToBody(currentBody) });
  }
  if (!seenUnreleased) {
    throw new Error("CHANGELOG.md must contain a ## [Unreleased] section");
  }

  return {
    raw,
    preamble: linesToBody(preamble),
    unreleasedBody: linesToBody(unreleasedBody),
    sections,
  };
}

export function unreleasedHasContent(log: Changelog): boolean {
  return log.unreleasedBody.trim().split(/\r?\n/).some((line) => {
    const trimmed = line.trim();
    return trimmed.length > 0 && !trimmed.startsWith("### ");
  });
}

export function sectionForVersion(log: Changelog, version: string): ChangelogSection | undefined {
  const needle = `## [${version}]`;
  return log.sections.find((section) => section.heading.startsWith(needle));
}

export function validateReleaseChangelog(log: Changelog, version: string): string[] {
  const errors: string[] = [];
  const section = sectionForVersion(log, version);
  if (!section) {
    errors.push(`CHANGELOG.md missing ## [${version}] section`);
  } else if (section.body.trim().length === 0) {
    errors.push(`CHANGELOG.md ## [${version}] must not be empty`);
  }
  if (!log.raw.includes("## [Unreleased]")) {
    errors.push("CHANGELOG.md must include a ## [Unreleased] section");
  }
  if (unreleasedHasContent(log)) {
    errors.push("CHANGELOG.md [Unreleased] must be empty in a release PR");
  }
  return errors;
}

export function finalizeUnreleased(log: Changelog, version: string, today: () => string = todayUtc): Changelog {
  if (!unreleasedHasContent(log)) {
    throw new Error("CHANGELOG.md [Unreleased] has no release note bullets");
  }
  const heading = `## [${version}] - ${today()}`;
  const newSection: ChangelogSection = { heading, body: log.unreleasedBody };
  const next = {
    preamble: log.preamble,
    unreleasedBody: "",
    sections: [newSection, ...log.sections],
  };
  return { ...next, raw: renderChangelog(next) };
}

/** Return the changelog with its [Unreleased] body replaced and re-rendered. */
export function withUnreleased(log: Changelog, unreleasedBody: string): Changelog {
  const next = { preamble: log.preamble, unreleasedBody, sections: log.sections };
  return { ...next, raw: renderChangelog(next) };
}

function renderChangelog(log: Pick<Changelog, "preamble" | "unreleasedBody" | "sections">): string {
  const blocks: string[] = [];
  const preamble = log.preamble.trimEnd();
  if (preamble.length > 0) blocks.push(preamble);

  const unreleased = log.unreleasedBody.trimEnd();
  blocks.push(unreleased.length > 0 ? `## [Unreleased]\n${unreleased}` : "## [Unreleased]");

  for (const section of log.sections) {
    const body = section.body.trimEnd();
    blocks.push(body.length > 0 ? `${section.heading}\n${body}` : section.heading);
  }

  return `${blocks.join("\n\n")}\n`;
}

function linesToBody(lines: string[]): string {
  return lines.length === 0 ? "" : `${lines.join("\n")}\n`;
}

function todayUtc(): string {
  const now = new Date();
  return `${now.getUTCFullYear()}-${String(now.getUTCMonth() + 1).padStart(2, "0")}-${String(now.getUTCDate()).padStart(2, "0")}`;
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
    const heading = subHeading(line);
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

function subHeading(line: string): string | undefined {
  return line.startsWith("### ") ? line.slice(4).trim() : undefined;
}

function bulletBody(line: string): string | undefined {
  const m = line.match(/^\s*-\s+(.*)$/);
  return m ? m[1]!.trim() : undefined;
}

function normalizeBullet(line: string): string {
  return (bulletBody(line) ?? line).trim().toLowerCase().replace(/\s+/g, " ");
}
