---
title: CI and headless
description: Configure static-token auth and read projected paths in non-interactive environments.
---

# CI and headless

**Supply a token without an interactive login**

A mount can read its credential from an environment variable or a file instead of the interactive store, which is what CI needs:

    {
      "provider": "omnifs_provider_github.wasm",
      "mount": "github",
      "auth": { "type": "static-token", "scheme": "pat", "token_env": "GITHUB_TOKEN" }
    }

The host injects the token from `GITHUB_TOKEN`, and nothing prompts.

**Read a path in a headless job**

    omnifs up
    docker exec omnifs /bin/zsh -lc 'cat /omnifs/github/rust-lang/rust/issues/open/12345/title'
    omnifs down

This needs no shell session and no browser. Host-managed OAuth still needs an interactive first login, so CI uses static tokens through `token_env` or `token_file`, not the OAuth flows.
