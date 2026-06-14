// Curated release-note areas. This is the single source of truth for both the
// LLM drafter (which classifies each bullet into one area) and the changelog
// renderer (which emits one `### <heading>` subsection per area, in this order).
//
// Keep this list short and coarse. Areas are product-facing buckets, not commit
// types and not internal crate names. When in doubt a bullet lands in the
// trailing catch-all area rather than spawning a new one.

export type AreaId =
  | "providers"
  | "runtime"
  | "cli"
  | "caching"
  | "auth"
  | "packaging";

export type Area = {
  id: AreaId;
  /** The `### ` subsection heading written into CHANGELOG.md and the PR comment. */
  heading: string;
  /** Lowercase strings that classify free-text area labels onto this area. */
  aliases: string[];
};

// Order here is the order subsections appear in the changelog. The last entry
// is the catch-all for anything unclassifiable.
export const AREAS: readonly Area[] = [
  {
    id: "providers",
    heading: "Providers & projected paths",
    aliases: ["provider", "providers", "projected paths", "paths", "routing", "tree"],
  },
  {
    id: "runtime",
    heading: "Runtime & mounts",
    aliases: ["runtime", "daemon", "omnifsd", "mount", "mounts", "fuse", "container"],
  },
  {
    id: "cli",
    heading: "CLI & workflow",
    aliases: ["cli", "command", "commands", "workflow", "omnifs dev", "shell"],
  },
  {
    id: "caching",
    heading: "Caching & performance",
    aliases: ["cache", "caching", "performance", "perf", "speed", "memory"],
  },
  {
    id: "auth",
    heading: "Auth & credentials",
    aliases: ["auth", "authentication", "credential", "credentials", "oauth", "token", "clone"],
  },
  {
    id: "packaging",
    heading: "Packaging & release",
    aliases: ["packaging", "package", "npm", "release", "docker", "image", "ghcr", "install"],
  },
] as const;

/** Area ids in canonical (changelog) order, as a non-empty tuple for `z.enum`. */
export const AREA_IDS = AREAS.map((a) => a.id) as [AreaId, ...AreaId[]];

const BY_ID = new Map<AreaId, Area>(AREAS.map((a) => [a.id, a]));

export function areaById(id: AreaId): Area {
  const area = BY_ID.get(id);
  if (!area) throw new Error(`unknown area id: ${id}`);
  return area;
}

/**
 * Resolve a free-text area label (an id, a heading, or an alias; any case) to a
 * canonical area. Used when parsing the LLM draft and the editable PR comment,
 * where humans may type a heading or a loose synonym. Returns undefined when
 * nothing matches so callers can decide between the catch-all and an error.
 */
export function resolveArea(label: string): Area | undefined {
  const needle = label.trim().toLowerCase();
  if (needle.length === 0) return undefined;
  for (const area of AREAS) {
    if (area.id === needle) return area;
    if (area.heading.toLowerCase() === needle) return area;
    if (area.aliases.includes(needle)) return area;
  }
  return undefined;
}

/** Index of an area in canonical order; used to sort subsections deterministically. */
export function areaIndex(id: AreaId): number {
  return AREA_IDS.indexOf(id);
}
