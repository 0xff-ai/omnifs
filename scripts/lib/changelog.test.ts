import { describe, expect, test } from "bun:test";
import {
  appendBulletsToUnreleased,
  finalizeUnreleased,
  parseChangelog,
  sectionForVersion,
  unreleasedHasContent,
  validateReleaseChangelog,
  type AreaBullet,
} from "./changelog";

const FIXED_DATE = () => "2026-05-26";

const PREAMBLE = `# Changelog

All notable changes to this project will be documented in this file.
`;

function changelog(body: string): string {
  return PREAMBLE + "\n" + body;
}

describe("parseChangelog", () => {
  test("requires [Unreleased]", () => {
    expect(() => parseChangelog(changelog("## [1.0.0] - 2025-01-01\n\n### Added\n- foo\n")))
      .toThrow(/must contain a ## \[Unreleased\] section/);
  });

  test("rejects duplicate [Unreleased]", () => {
    expect(() => parseChangelog(changelog("## [Unreleased]\n\n## [Unreleased]\n")))
      .toThrow(/only one ## \[Unreleased\]/);
  });

  test("captures preamble, unreleased body, and prior sections", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n- new thing\n\n## [1.0.0] - 2025-01-01\n\n### Fixed\n- prior bug\n"));
    expect(log.preamble).toContain("# Changelog");
    expect(log.unreleasedBody).toContain("### Added");
    expect(log.unreleasedBody).toContain("- new thing");
    expect(log.sections.length).toBe(1);
    expect(log.sections[0]?.heading).toBe("## [1.0.0] - 2025-01-01");
    expect(log.sections[0]?.body).toContain("- prior bug");
  });
});

describe("unreleasedHasContent", () => {
  test("returns false when only headings are present", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n\n### Fixed\n"));
    expect(unreleasedHasContent(log)).toBe(false);
  });

  test("returns false when section is empty", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n"));
    expect(unreleasedHasContent(log)).toBe(false);
  });

  test("returns true when a bullet is present", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n- something real\n"));
    expect(unreleasedHasContent(log)).toBe(true);
  });
});

describe("sectionForVersion", () => {
  test("matches an exact version", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n## [1.2.3] - 2025-02-02\n\n### Fixed\n- a\n"));
    expect(sectionForVersion(log, "1.2.3")?.heading).toBe("## [1.2.3] - 2025-02-02");
  });

  test("returns undefined for missing versions", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n## [1.2.3] - 2025-02-02\n\n### Fixed\n- a\n"));
    expect(sectionForVersion(log, "0.9.9")).toBeUndefined();
  });
});

describe("finalizeUnreleased", () => {
  test("rejects empty [Unreleased]", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n"));
    expect(() => finalizeUnreleased(log, "1.0.0", FIXED_DATE))
      .toThrow(/no release note bullets/);
  });

  test("moves bullets into a new dated version section and empties [Unreleased]", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n- new feature\n\n## [0.9.0] - 2024-12-01\n\n### Fixed\n- prior bug\n"));
    const finalized = finalizeUnreleased(log, "1.0.0", FIXED_DATE);

    expect(unreleasedHasContent(finalized)).toBe(false);
    expect(finalized.unreleasedBody).toBe("");
    expect(finalized.sections[0]?.heading).toBe("## [1.0.0] - 2026-05-26");
    expect(finalized.sections[0]?.body).toContain("- new feature");
    expect(finalized.sections[1]?.heading).toBe("## [0.9.0] - 2024-12-01");
    expect(finalized.sections[1]?.body).toContain("- prior bug");
  });

  test("renders deterministic raw output that round-trips through parseChangelog", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n- new\n\n## [0.9.0] - 2024-12-01\n\n### Fixed\n- old\n"));
    const finalized = finalizeUnreleased(log, "1.0.0", FIXED_DATE);
    const reparsed = parseChangelog(finalized.raw);

    expect(reparsed.sections.map((s) => s.heading)).toEqual([
      "## [1.0.0] - 2026-05-26",
      "## [0.9.0] - 2024-12-01",
    ]);
    expect(unreleasedHasContent(reparsed)).toBe(false);
  });
});

describe("validateReleaseChangelog", () => {
  test("flags missing version section", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n## [0.9.0] - 2024-12-01\n\n### Fixed\n- old\n"));
    expect(validateReleaseChangelog(log, "1.0.0")).toContain("CHANGELOG.md missing ## [1.0.0] section");
  });

  test("flags empty version section", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n## [1.0.0] - 2025-01-01\n"));
    const errors = validateReleaseChangelog(log, "1.0.0");
    expect(errors.some((e) => e.includes("## [1.0.0] must not be empty"))).toBe(true);
  });

  test("flags [Unreleased] still carrying content", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n### Added\n- leaked\n\n## [1.0.0] - 2025-01-01\n\n### Added\n- real\n"));
    const errors = validateReleaseChangelog(log, "1.0.0");
    expect(errors.some((e) => e.includes("[Unreleased] must be empty"))).toBe(true);
  });

  test("returns no errors on a healthy release log", () => {
    const log = parseChangelog(changelog("## [Unreleased]\n\n## [1.0.0] - 2025-01-01\n\n### Added\n- real\n"));
    expect(validateReleaseChangelog(log, "1.0.0")).toEqual([]);
  });
});

describe("appendBulletsToUnreleased", () => {
  test("groups a fresh set in canonical area order", () => {
    const body = appendBulletsToUnreleased("", [
      { area: "packaging", text: "npm dev tag" },
      { area: "providers", text: "new provider path" },
    ]);
    expect(body).toBe(
      "### Providers & projected paths\n- new provider path\n\n### Packaging & release\n- npm dev tag\n",
    );
  });

  test("appends to an existing area subsection without rewriting it", () => {
    const start = "### Providers & projected paths\n- existing bullet\n";
    const merged = appendBulletsToUnreleased(start, [{ area: "providers", text: "added bullet" }]);
    expect(merged).toBe("### Providers & projected paths\n- existing bullet\n- added bullet\n");
  });

  test("inserts a new area subsection at its canonical position", () => {
    const start = "### Packaging & release\n- pkg note\n";
    const merged = appendBulletsToUnreleased(start, [{ area: "providers", text: "prov note" }]);
    // Providers (index 0) must precede Packaging (index 5).
    expect(merged).toBe(
      "### Providers & projected paths\n- prov note\n\n### Packaging & release\n- pkg note\n",
    );
  });

  test("is idempotent on exact-duplicate bullets (recording the same entry twice is a no-op)", () => {
    const start = "### Auth & credentials\n- token refresh fixed\n";
    const merged = appendBulletsToUnreleased(start, [{ area: "auth", text: "token refresh fixed" }]);
    expect(merged).toBe(start);
  });

  test("preserves a human-edited bullet verbatim while adding a new one", () => {
    const start = "### Runtime & mounts\n- Hand-reworded line a maintainer typed.\n";
    const bullets: AreaBullet[] = [{ area: "runtime", text: "auto bullet" }];
    const merged = appendBulletsToUnreleased(start, bullets);
    expect(merged).toContain("- Hand-reworded line a maintainer typed.");
    expect(merged).toContain("- auto bullet");
  });
});
