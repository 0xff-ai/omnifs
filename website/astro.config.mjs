// @ts-check
import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import mermaid from "astro-mermaid";

// https://astro.build/config
export default defineConfig({
  site: "https://omnifs.dev",
  integrations: [
    // astro-mermaid renders ```mermaid fenced code blocks client-side and
    // follows Starlight's light/dark theme automatically. Must be listed
    // BEFORE the starlight() integration so its remark plugin runs first.
    mermaid({
      theme: "default",
      autoTheme: true,
      mermaidConfig: {
        flowchart: { curve: "basis", useMaxWidth: true },
        sequence: { useMaxWidth: true, mirrorActors: false },
        themeVariables: {
          fontFamily:
            "ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif",
        },
      },
    }),
    starlight({
      title: "omnifs",
      tagline: "The universe, mounted on your filesystem.",
      description:
        "omnifs mirrors external services into local paths via FUSE: GitHub, DNS, arXiv and more as files you can cd, ls, cat, and grep.",
      logo: {
        light: "./src/assets/logo-light.svg",
        dark: "./src/assets/logo-dark.svg",
        replacesTitle: false,
      },
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/0xff-ai/omnifs",
        },
      ],
      editLink: {
        baseUrl:
          "https://github.com/0xff-ai/omnifs/edit/main/website/",
      },
      customCss: ["./src/styles/custom.css"],
      lastUpdated: true,
      sidebar: [
        {
          label: "Introduction",
          items: [
            { label: "What is omnifs", slug: "introduction/what-is-omnifs" },
            { label: "Why omnifs", slug: "introduction/why-omnifs" },
            { label: "How it works", slug: "introduction/how-it-works" },
            { label: "Project status", slug: "introduction/project-status" },
          ],
        },
        {
          label: "Getting started",
          items: [
            { label: "Prerequisites", slug: "getting-started/prerequisites" },
            { label: "Install", slug: "getting-started/install" },
            { label: "Quickstart", slug: "getting-started/quickstart" },
            {
              label: "Guided onboarding",
              slug: "getting-started/guided-onboarding",
            },
            {
              label: "Platform notes",
              slug: "getting-started/platform-notes",
            },
          ],
        },
        {
          label: "Guides",
          items: [
            { label: "Overview", slug: "guides" },
            { label: "Browsing the filesystem", slug: "guides/browsing" },
            { label: "Working with GitHub", slug: "guides/github" },
            { label: "Querying DNS", slug: "guides/dns" },
            { label: "Browsing arXiv", slug: "guides/arxiv" },
            { label: "Using omnifs with agents", slug: "guides/agents" },
            { label: "Authenticating providers", slug: "guides/authentication" },
            { label: "Managing mounts", slug: "guides/managing-mounts" },
            {
              label: "Container lifecycle",
              slug: "guides/container-lifecycle",
            },
            { label: "Inspect & debug", slug: "guides/inspect-debug" },
            { label: "Troubleshooting", slug: "guides/troubleshooting" },
          ],
        },
        {
          label: "Providers",
          items: [
            { label: "Provider catalog", slug: "providers" },
            { label: "GitHub", slug: "providers/github" },
            { label: "DNS", slug: "providers/dns" },
            { label: "arXiv", slug: "providers/arxiv" },
            { label: "Database", slug: "providers/database" },
            { label: "Docker", slug: "providers/docker" },
            { label: "Linear", slug: "providers/linear" },
            { label: "Roadmap", slug: "providers/roadmap" },
          ],
        },
        {
          label: "CLI reference",
          items: [
            { label: "Overview & global flags", slug: "cli" },
            { label: "Lifecycle", slug: "cli/lifecycle" },
            { label: "Onboarding & config", slug: "cli/onboarding-config" },
            { label: "Auth", slug: "cli/auth" },
            { label: "Diagnostics", slug: "cli/diagnostics" },
            { label: "Contributor (dev)", slug: "cli/dev" },
          ],
        },
        {
          label: "Concepts",
          items: [
            { label: "Architecture overview", slug: "concepts/architecture" },
            { label: "The single path space", slug: "concepts/path-space" },
            { label: "Provider model", slug: "concepts/provider-model" },
            { label: "Callout runtime", slug: "concepts/callout-runtime" },
            {
              label: "Path dispatch & listing",
              slug: "concepts/path-dispatch",
            },
            { label: "Caching model", slug: "concepts/caching" },
            { label: "File attributes", slug: "concepts/file-attributes" },
            { label: "Auth & credentials", slug: "concepts/auth-credentials" },
            { label: "Cloning", slug: "concepts/cloning" },
            { label: "WASM sandbox substrate", slug: "concepts/wasm-sandbox" },
            { label: "Mount lifecycle", slug: "concepts/mount-lifecycle" },
          ],
        },
        {
          label: "Building providers",
          items: [
            { label: "Overview", slug: "building-providers/overview" },
            { label: "Project setup", slug: "building-providers/project-setup" },
            { label: "Handlers", slug: "building-providers/handlers" },
            { label: "Typed subtrees", slug: "building-providers/subtrees" },
            { label: "Config", slug: "building-providers/config" },
            { label: "Projections", slug: "building-providers/projections" },
            {
              label: "Project everything you fetched",
              slug: "building-providers/project-everything",
            },
            { label: "Auth manifest", slug: "building-providers/auth-manifest" },
            { label: "Callouts", slug: "building-providers/callouts" },
            {
              label: "Cache invalidation",
              slug: "building-providers/cache-invalidation",
            },
            { label: "Testing providers", slug: "building-providers/testing" },
            { label: "WIT reference", slug: "building-providers/wit-reference" },
          ],
        },
        {
          label: "Contributing",
          items: [
            { label: "Repo layout & crates", slug: "contributing/repo-layout" },
            { label: "Dev workflow", slug: "contributing/dev-workflow" },
            {
              label: "Build & validation",
              slug: "contributing/build-validation",
            },
            { label: "Testing", slug: "contributing/testing" },
            { label: "Observability", slug: "contributing/observability" },
            { label: "Coding conventions", slug: "contributing/conventions" },
          ],
        },
        {
          label: "Releasing & distribution",
          items: [
            { label: "Release process", slug: "releasing/process" },
            { label: "Version coupling", slug: "releasing/version-coupling" },
            { label: "npm packaging", slug: "releasing/npm" },
            { label: "Runtime image", slug: "releasing/runtime-image" },
            { label: "Native CI pipeline", slug: "releasing/native-ci" },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Mount config schema", slug: "reference/mount-schema" },
            { label: "Glossary", slug: "reference/glossary" },
            { label: "Roadmap", slug: "reference/roadmap" },
            { label: "FAQ", slug: "reference/faq" },
            { label: "Future & RFCs", slug: "reference/future" },
          ],
        },
      ],
    }),
  ],
});
