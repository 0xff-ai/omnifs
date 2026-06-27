set shell := ["bash", "-euo", "pipefail", "-c"]

# Show the maintainer command menu.
[default]
default:
    @just --justfile '{{ justfile() }}' --list --unsorted

import 'just/dev.just'
mod providers 'just/providers.just'
mod host 'just/host.just'
mod npm 'just/npm.just'
