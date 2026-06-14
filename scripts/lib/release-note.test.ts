import { describe, expect, test } from "bun:test";
import type { ChangelogDraft } from "./llm";
import {
  lengthForReaction,
  parseOptionsComment,
  renderOptionsComment,
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
