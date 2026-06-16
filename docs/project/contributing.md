# Contributing

The contributor workflow runs through `omnifs dev`, which requires a source checkout.

```
omnifs dev          # build the dev image, materialize fixtures, launch the container
omnifs shell        # attach a shell
omnifs logs -f      # follow output
omnifs status       # inspect mounts, providers, auth
omnifs down         # stop and remove the container
```

`omnifs dev` finds the workspace, captures `gh auth token` into a mounted secret, downloads the SQLite fixture, builds an image tagged with the short SHA, and starts the container with all built-in providers mounted under `/omnifs`.

Validation has two levels. For host or CLI changes, the Rust baseline is `cargo fmt` and `cargo nextest run`. For broad surfaces or provider code, the repo checks are `just check`, `just providers-check`, and `just providers-build`. Provider commands target `wasm32-wasip2`, and provider tests compile but cannot run on the host, so use `--no-run`.

For mount, provider, clone, traversal, or runtime changes, do not stop at Rust checks. Validate through the live runtime: bring up `omnifs dev`, walk the affected provider tree with `ll`, `cd`, and `find` from the root through every intermediate directory, and read the runtime log. The bash-tool compatibility list is the contract. A change that regresses `tar`, `find`, or any tool on it is wrong.
