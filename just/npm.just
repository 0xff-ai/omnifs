# Synchronize npm package versions from the Cargo workspace version.
[group('npm')]
npm-sync *args:
    bun scripts/npm.ts sync {{ args }}

# Validate npm platform metadata and package manifests.
[group('npm')]
npm-validate:
    bun scripts/npm.ts validate

# Pack the root npm package locally. Destination defaults to $TMPDIR or /tmp.
[group('npm')]
npm-pack-root destination=env_var_or_default('TMPDIR', '/tmp'):
    npm pack ./npm/omnifs --pack-destination '{{ destination }}'
