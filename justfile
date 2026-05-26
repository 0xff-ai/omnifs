set shell := ["bash", "-euo", "pipefail", "-c"]

# Show the maintainer command menu.
[default]
default:
    @just --justfile '{{ justfile() }}' --list --unsorted

import 'just/dev.just'
import 'just/providers.just'
import 'just/npm.just'
import 'just/release.just'
import 'just/ci.just'
