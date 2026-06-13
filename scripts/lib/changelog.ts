export type ChangelogSection = {
  heading: string;
  body: string;
};

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

/**
 * Build a changelog whose [Unreleased] is empty and whose top version section is
 * `## [version] - date` with `body`, replacing any existing same-version section
 * and preserving older ones. Used by the standing release PR maintainer, which
 * accumulates folded notes directly in the version section (not [Unreleased]).
 */
export function withReleaseSection(
  log: Changelog,
  version: string,
  body: string,
  date: string,
): Changelog {
  const heading = `## [${version}] - ${date}`;
  const others = log.sections.filter((section) => !section.heading.startsWith(`## [${version}]`));
  const next = {
    preamble: log.preamble,
    unreleasedBody: "",
    sections: [{ heading, body }, ...others],
  };
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
