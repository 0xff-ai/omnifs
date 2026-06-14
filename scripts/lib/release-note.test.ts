import { describe, expect, test } from "bun:test";
import type { ChangelogDraft } from "./llm";
import {
  appendBulletsToUnreleased,
  lengthForReaction,
  parseOptionsComment,
  renderOptionsComment,
  type AreaBullet,
} from "./release-note";

const draft: ChangelogDraft = {
  skip: false,
  area: "providers",
  short: "arXiv stops crashing",
  medium: "The arXiv provider no longer crashes on malformed entries.",
  long: "The arXiv provider no longer crashes when an entry is missing its abstract; the path now renders an empty file instead of erroring.",
};

describe("options comment", () => {
  test("round-trips a draft through render then parse", () => {
    const parsed = parseOptionsComment(renderOptionsComment(draft));
    expect(parsed).toEqual(draft);
  });

  test("round-trips a skip draft", () => {
    const skip: ChangelogDraft = { ...draft, skip: true, short: "", medium: "", long: "" };
    expect(parseOptionsComment(renderOptionsComment(skip))).toEqual(skip);
  });

  test("round-trips text containing the comment terminator and unicode", () => {
    // "-->" in the body must not truncate the (base64) data block; unicode must survive.
    const tricky: ChangelogDraft = { ...draft, medium: "routes /a --> /b now resolve — café ☕" };
    expect(parseOptionsComment(renderOptionsComment(tricky))).toEqual(tricky);
  });

  test("renders the three length options with their selector emoji", () => {
    const body = renderOptionsComment(draft);
    expect(body).toContain(`👍 short: ${draft.short}`);
    expect(body).toContain(`🚀 medium: ${draft.medium}`);
    expect(body).toContain(`👀 long: ${draft.long}`);
  });

  test("returns undefined for a comment without the marker", () => {
    expect(parseOptionsComment("just a normal review comment")).toBeUndefined();
  });
});

describe("lengthForReaction", () => {
  test("maps the three selector reactions and ignores others", () => {
    expect(lengthForReaction("+1")).toBe("short");
    expect(lengthForReaction("rocket")).toBe("medium");
    expect(lengthForReaction("eyes")).toBe("long");
    expect(lengthForReaction("heart")).toBeUndefined();
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
