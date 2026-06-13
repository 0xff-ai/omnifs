import { describe, expect, test } from "bun:test";
import {
  MARKER,
  appendBulletsToUnreleased,
  parseReleaseNoteComment,
  renderReleaseNoteComment,
  type AreaBullet,
} from "./release-note";

describe("parseReleaseNoteComment", () => {
  test("returns null without the marker", () => {
    expect(parseReleaseNoteComment("just a normal review comment")).toBeNull();
  });

  test("parses bullets grouped by area heading", () => {
    const note = parseReleaseNoteComment(
      `${MARKER}\n\n### Providers & projected paths\n- arXiv stops crashing\n- new path\n\n### Caching & performance\n- lower memory`,
    );
    expect(note?.skip).toBe(false);
    expect(note?.bullets).toEqual([
      { area: "providers", text: "arXiv stops crashing" },
      { area: "providers", text: "new path" },
      { area: "caching", text: "lower memory" },
    ]);
  });

  test("resolves loose area aliases and records unknown headings", () => {
    const note = parseReleaseNoteComment(`${MARKER}\n### npm\n- dev tag\n### Whatever\n- orphan`);
    expect(note?.bullets[0]).toEqual({ area: "packaging", text: "dev tag" });
    // Unknown heading folds into the trailing catch-all area, not dropped.
    expect(note?.bullets[1]).toEqual({ area: "packaging", text: "orphan" });
    expect(note?.unknownHeadings).toContain("Whatever");
  });

  test("detects the skip opt-out", () => {
    const note = parseReleaseNoteComment(renderReleaseNoteComment({ skip: true, bullets: [], unknownHeadings: [] }));
    expect(note?.skip).toBe(true);
    expect(note?.bullets).toEqual([]);
  });

  test("round-trips a drafted note through render then parse", () => {
    const bullets: AreaBullet[] = [
      { area: "runtime", text: "daemon owns the mount" },
      { area: "providers", text: "object routing" },
    ];
    const reparsed = parseReleaseNoteComment(renderReleaseNoteComment({ skip: false, bullets, unknownHeadings: [] }));
    // Rendering groups in canonical area order: providers precedes runtime.
    expect(reparsed?.bullets).toEqual([
      { area: "providers", text: "object routing" },
      { area: "runtime", text: "daemon owns the mount" },
    ]);
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

  test("is idempotent on exact-duplicate bullets (re-folding a PR is a no-op)", () => {
    const start = "### Auth & credentials\n- token refresh fixed\n";
    const merged = appendBulletsToUnreleased(start, [{ area: "auth", text: "token refresh fixed" }]);
    expect(merged).toBe(start);
  });

  test("preserves a human-edited bullet verbatim while adding a new one", () => {
    const start = "### Runtime & mounts\n- Hand-reworded line a maintainer typed.\n";
    const merged = appendBulletsToUnreleased(start, [{ area: "runtime", text: "auto bullet" }]);
    expect(merged).toContain("- Hand-reworded line a maintainer typed.");
    expect(merged).toContain("- auto bullet");
  });
});
