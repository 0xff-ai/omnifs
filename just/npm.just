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

    packages=()
    while IFS= read -r package_json; do
      packages+=("$(jq -er '.name' "$package_json")")
    done < <(find npm/platform -mindepth 2 -maxdepth 2 -name package.json | sort)

    while IFS= read -r package; do
      npm pkg delete "optionalDependencies[${package}]" --prefix npm --workspace @0xff-ai/omnifs
    done < <(jq -r '(.optionalDependencies // {}) | keys[]' npm/omnifs/package.json)

    optional_dependencies=()
    for package in "${packages[@]}"; do
      optional_dependencies+=("optionalDependencies[${package}]=${version}")
    done
    npm pkg set "${optional_dependencies[@]}" --prefix npm --workspace @0xff-ai/omnifs

    printf 'synced npm packages to version %s\n' "$version"

# Pack the root npm package locally. Destination defaults to $TMPDIR or /tmp.
pack destination=env_var_or_default('TMPDIR', '/tmp'):
    npm pack ./npm/omnifs --pack-destination '{{ destination }}'
