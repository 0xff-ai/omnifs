set shell := ["bash", "-euo", "pipefail", "-c"]
set working-directory := '..'

# Synchronize npm package versions from the Cargo workspace version.
sync version='':
    #!/usr/bin/env bash
    set -euo pipefail

    version="{{ version }}"
    if [[ -z "$version" ]]; then
      version="$(cargo metadata --no-deps --format-version 1 \
        | jq -er '.packages[] | select(.name == "omnifs-cli") | .version')"
    fi

    npm pkg set "version=${version}" --prefix npm --workspaces

    mapfile -t packages < <(jq -r '.[] | .package' npm/platforms.json)
    optional_dependencies=()
    for package in "${packages[@]}"; do
      optional_dependencies+=("optionalDependencies[${package}]=${version}")
    done
    npm pkg set "${optional_dependencies[@]}" --prefix npm --workspace @0xff-ai/omnifs

    printf 'synced npm packages to version %s\n' "$version"

# Validate npm platform metadata and package manifests.
validate:
    cargo xtask npm validate

# Pack the root npm package locally. Destination defaults to $TMPDIR or /tmp.
pack destination=env_var_or_default('TMPDIR', '/tmp'):
    npm pack ./npm/omnifs --pack-destination '{{ destination }}'
