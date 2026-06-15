// Sidebar/nav structure for the omnifs docs.
//
// This is the source of truth for documentation information architecture: group
// order and labels live here, page order within a group is controlled by each
// page's frontmatter. The website's Starlight config imports this after syncing
// `docs/` into its content collection; the website owns presentation, not IA.
//
// The leading underscore keeps Astro/Starlight from treating this file as a
// content page while still allowing a direct import.

export const sidebar = [
  { label: "Get oriented", autogenerate: { directory: "get-oriented" } },
  { label: "Projections", autogenerate: { directory: "projections" } },
  { label: "Engine", autogenerate: { directory: "engine" } },
  { label: "Providers", autogenerate: { directory: "providers" } },
  { label: "Surfaces", autogenerate: { directory: "surfaces" } },
  { label: "Recipes", autogenerate: { directory: "recipes" } },
  { label: "Tutorials", autogenerate: { directory: "tutorials" } },
  { label: "Security and trust", autogenerate: { directory: "security" } },
  { label: "Reference", autogenerate: { directory: "reference" } },
  { label: "Project", autogenerate: { directory: "project" } },
];
