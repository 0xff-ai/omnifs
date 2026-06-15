---
title: Make and pipelines
description: Drive make and shell pipelines from projected paths with real mtimes.
---

# Make and pipelines

**Rebuild only what changed upstream**

    report.md: /omnifs/db/tables/orders/count.txt /omnifs/linear/teams/ENG/issues/open
    	./render.sh $^ > $@

`make` is an mtime-aware orchestrator, and projected paths have real mtimes. Run `make report.md` twice and the second run rebuilds only the sections whose upstream paths changed.

Piping a prompt into a model-as-file, so a makefile recipe writes to `/llm/.../jobs/` and reads the completion, is the same pattern with an LLM provider. That provider is on the roadmap. The makefile shape works today over the providers that ship.
