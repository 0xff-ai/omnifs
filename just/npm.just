set shell := ["bash", "-euo", "pipefail", "-c"]
set working-directory := '..'

# Synchronize npm package versions from the Cargo workspace version.
sync *args:
    cargo xtask npm sync {{ args }}

# Validate npm platform metadata and package manifests.
validate:
    cargo xtask npm validate

# Pack the root npm package locally. Destination defaults to $TMPDIR or /tmp.
pack destination=env_var_or_default('TMPDIR', '/tmp'):
    npm pack ./npm/omnifs --pack-destination '{{ destination }}'
