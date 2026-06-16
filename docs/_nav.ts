// Sidebar/nav structure for the omnifs docs.
//
// This is the source of truth for documentation information architecture: both
// the section order/labels and the page order within each section live here, as
// explicit ordered `items` lists. Bare strings are content-collection slugs; each
// page's sidebar label comes from its own `title` frontmatter. The website's
// Starlight config imports this after syncing `docs/` into its content collection;
// the website owns presentation, not IA.
//
// Adding a page means adding its slug here in the position it should appear; a
// missing or misspelled slug fails the website build rather than silently
// dropping or misordering the page.
//
// The leading underscore keeps Astro/Starlight from treating this file as a
// content page while still allowing a direct import.

export const sidebar = [
  {
    label: "Get oriented",
    items: [
      "get-oriented/what-omnifs-is",
      "get-oriented/why-paths",
      "get-oriented/use-cases",
      "get-oriented/install-and-first-read",
      "get-oriented/setup-and-troubleshooting",
      "get-oriented/limits",
    ],
  },
  {
    label: "Projections",
    items: [
      "projections/paths-as-the-interface",
      "projections/the-browse-surface",
      "projections/subtree-handoff",
      "projections/what-files-report",
    ],
  },
  {
    label: "Engine",
    items: [
      "engine/the-cache-trilogy",
      "engine/callouts-and-effects",
      "engine/auth-and-credential-custody",
    ],
  },
  {
    label: "Providers",
    items: [
      "providers",
      "providers/the-two-flavours",
      "providers/authoring-guide",
      "providers/routing-and-objects",
      "providers/config-manifests-and-capabilities",
      "providers/reaching-upstream",
      "providers/testing-and-debugging",
      "providers/packaging",
    ],
  },
  {
    label: "Surfaces",
    items: ["surfaces/shell-compatibility"],
  },
  {
    label: "Recipes",
    items: [
      "recipes/read",
      "recipes/search",
      "recipes/stat",
      "recipes/copy-and-archive",
      "recipes/compare-and-hash",
      "recipes/structured",
      "recipes/cross-service-pipes",
      "recipes/make-and-pipelines",
      "recipes/ci-and-headless",
      "recipes/with-local-agents",
      "recipes/build-on-the-namespace",
    ],
  },
  {
    label: "Tutorials",
    items: [
      "tutorials/mount-github-and-inspect-issues",
      "tutorials/build-a-tiny-provider",
      "tutorials/add-auth-to-a-provider",
      "tutorials/add-object-and-view-caching",
      "tutorials/expose-a-local-sqlite-database",
    ],
  },
  {
    label: "Security and trust",
    items: ["security/the-trust-model"],
  },
  {
    label: "Reference",
    items: [
      "reference",
      "reference/cli",
      "reference/config-schema",
      "reference/paths",
      "reference/provider-manifest",
      "reference/runtime-grants",
      "reference/wit",
      "reference/file-attributes",
      "reference/sdk",
      "reference/cache",
      "reference/capabilities",
      "reference/errors",
      "reference/environment",
      "reference/glossary",
      "reference/shell-compatibility",
    ],
  },
  {
    label: "Project",
    items: [
      "project/roadmap",
      "project/distribution",
      "project/contributing",
      "project/design-decisions",
      "project/faq",
    ],
  },
];
