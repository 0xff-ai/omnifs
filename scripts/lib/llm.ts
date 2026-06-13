// Minimal Anthropic Messages client for drafting release notes. Dependency-free
// (uses fetch) so the scripts tree needs no SDK. Pinned model for determinism
// and cost control; override with OMNIFS_RELEASE_NOTES_MODEL.

const DEFAULT_MODEL = "claude-sonnet-4-6";
const API_URL = "https://api.anthropic.com/v1/messages";

export type LlmDraftOptions = {
  system: string;
  user: string;
  maxTokens?: number;
};

/** Single-shot completion. Returns the concatenated text content blocks. */
export async function llmComplete(opts: LlmDraftOptions): Promise<string> {
  const apiKey = process.env.ANTHROPIC_API_KEY;
  if (!apiKey) throw new Error("ANTHROPIC_API_KEY is not set");
  const model = process.env.OMNIFS_RELEASE_NOTES_MODEL || DEFAULT_MODEL;

  const res = await fetch(API_URL, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-api-key": apiKey,
      "anthropic-version": "2023-06-01",
    },
    body: JSON.stringify({
      model,
      max_tokens: opts.maxTokens ?? 1024,
      system: opts.system,
      messages: [{ role: "user", content: opts.user }],
    }),
  });

  if (!res.ok) {
    throw new Error(`Anthropic API ${res.status}: ${(await res.text()).slice(0, 500)}`);
  }
  const data = (await res.json()) as { content?: Array<{ type: string; text?: string }> };
  const text = (data.content ?? [])
    .filter((b) => b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .join("")
    .trim();
  if (text.length === 0) throw new Error("Anthropic API returned no text content");
  return text;
}
