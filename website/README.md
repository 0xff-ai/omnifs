# omnifs documentation site

The public documentation for [omnifs](https://github.com/0xff-ai/omnifs) — *the
universe, mounted on your filesystem.* Built with
[Astro Starlight](https://starlight.astro.build) with client-side
[Mermaid](https://mermaid.js.org) diagram rendering via
[`astro-mermaid`](https://github.com/wjayh/astro-mermaid).

## Develop

```bash
cd website
bun install        # or: npm install / pnpm install
bun run dev        # local dev server at http://localhost:4321
bun run build      # production build to ./dist
bun run preview    # preview the production build
```

> The content is **pure Markdown** under `src/content/docs/`. Diagrams are plain
> ` ```mermaid ` fenced code blocks; `astro-mermaid` renders them client-side and
> follows the site's light/dark theme automatically.

## Structure

```
website/
├── astro.config.mjs        # Starlight + Mermaid integration, sidebar
├── src/
│   ├── content.config.ts   # docs content collection
│   ├── content/docs/       # all documentation pages (Markdown)
│   ├── styles/custom.css   # brand theme tuning
│   └── assets/             # logos
└── package.json
```

## Authoring conventions

- Pages are self-contained Markdown with frontmatter (`title`, `description`).
- Use ` ```mermaid ` fenced blocks for diagrams; do not hard-code theme colors
  inside diagrams — global light/dark theming is handled by the integration.
- Use Starlight asides (`:::note`, `:::tip`, `:::caution`) for callouts.
- Path → content mappings are documented as Markdown tables.
