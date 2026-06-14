// Drafts changelog entries with the Vercel AI SDK against an OpenAI-compatible
// gateway (OpenCode Zen by default). Output is structured (generateObject + a Zod
// schema) rather than free text: more reliable on small/cheap models like
// DeepSeek Flash, and it removes any brittle parse step.
//
// The model and endpoint are config, not code: set OPENCODE_ZEN_API_KEY, and
// override OMNIFS_RELEASE_NOTES_MODEL / OMNIFS_RELEASE_NOTES_BASE_URL to point
// elsewhere. The default model slug should be confirmed against the gateway's
// model list.

import { createOpenAICompatible } from "@ai-sdk/openai-compatible";
import { generateObject } from "ai";
import { z } from "zod";
import { AREA_IDS } from "./areas";

const DEFAULT_BASE_URL = "https://opencode.ai/zen/v1";
const DEFAULT_MODEL = "opencode/deepseek-v4-flash";

const draftSchema = z.object({
  skip: z.boolean().describe("true when the PR has no user-facing change (chore, pure refactor, tests, CI)"),
  area: z.enum(AREA_IDS).describe("the single product area this change belongs to"),
  short: z.string().describe("one terse line, roughly 6-10 words; empty when skip"),
  medium: z.string().describe("one sentence, roughly 15-25 words; empty when skip"),
  long: z.string().describe("one or two sentences with the user-facing detail, roughly 30-50 words; empty when skip"),
});

export type ChangelogDraft = z.infer<typeof draftSchema>;

/** Draft one changelog entry in three lengths from a PR/commit title and diff. */
export async function draftChangelogOptions(title: string, diff: string): Promise<ChangelogDraft> {
  const apiKey = process.env.OPENCODE_ZEN_API_KEY;
  if (!apiKey) throw new Error("OPENCODE_ZEN_API_KEY is not set");

  const provider = createOpenAICompatible({
    name: "opencode",
    baseURL: process.env.OMNIFS_RELEASE_NOTES_BASE_URL || DEFAULT_BASE_URL,
    apiKey,
  });
  const model = provider(process.env.OMNIFS_RELEASE_NOTES_MODEL || DEFAULT_MODEL);

  const { object } = await generateObject({
    model,
    schema: draftSchema,
    system: SYSTEM_PROMPT,
    prompt: userPrompt(title, diff),
  });
  return object;
}

// "JSON" is named on purpose: DeepSeek's json_object response format requires the
// word to appear in the prompt.
const SYSTEM_PROMPT = `You write end-user changelog entries for omnifs, a projected filesystem that mirrors external services into local paths.

From a PR title and diff, produce a single changelog entry in three lengths (short, medium, long), all describing the same change in the same plain, user-facing style. Return JSON matching the provided schema.

Rules:
- Describe observable behavior, not implementation. No commit-type prefixes, no file names, no internal crate names.
- Pick the single best-fitting product area from the schema's enum.
- If the PR has no user-facing change (chore, pure refactor, tests, CI, dependency bumps), set skip=true and leave the text fields empty.`;

function userPrompt(title: string, diff: string): string {
  const MAX_DIFF = 60_000;
  const clipped = diff.length > MAX_DIFF ? `${diff.slice(0, MAX_DIFF)}\n... [diff truncated]` : diff;
  return `PR title: ${title}\n\nUnified diff:\n${clipped}`;
}
