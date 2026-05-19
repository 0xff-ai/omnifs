image := "ghcr.io/raulk/omnifs:latest"
container := "omnifs"
db_test_data := ""

check: build-providers
    cargo fmt --all --check
    cargo clippy -- -D warnings
    cargo test -p omnifs-cli -p omnifs-host -p omnifs-sdk -p omnifs-sdk-macros
    just check-providers

check-providers:
    cargo check -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider -p 'omnifs-tool-*' --target wasm32-wasip2
    cargo clippy -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider -p 'omnifs-tool-*' --target wasm32-wasip2 -- -D warnings
    cargo test -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider --target wasm32-wasip2 --no-run

build-providers:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --target wasm32-wasip2 --release \
        -p omnifs-provider-arxiv -p omnifs-provider-db -p omnifs-provider-docker -p omnifs-provider-github -p omnifs-provider-dns -p omnifs-provider-linear -p test-provider \
        -p 'omnifs-tool-*'

test: build-providers
    cargo test --workspace

test-integration: build-providers
    cargo test -p omnifs-host --test runtime_test

build:
    docker build -t {{image}} .

dev:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .secrets
    if [[ ! -s .secrets/github_token ]]; then
      umask 077
      gh auth token > .secrets/github_token
    fi
    if [[ ! -s .secrets/chinook.sqlite ]]; then
      umask 077
      curl -fsSL -o .secrets/chinook.sqlite \
        https://raw.githubusercontent.com/lerocha/chinook-database/master/ChinookDatabase/DataSources/Chinook_Sqlite.sqlite
    fi
    export GITHUB_TOKEN_FILE="$(pwd)/.secrets/github_token"
    export OMNIFS_DB_FIXTURE="$(pwd)/.secrets/chinook.sqlite"
    docker compose up --build -d

start:
    #!/usr/bin/env bash
    set -euo pipefail
    export GITHUB_TOKEN="${GITHUB_TOKEN:-$(gh auth token)}"
    : "${SSH_AUTH_SOCK:?SSH_AUTH_SOCK must be set on the host}"
    docker rm -f {{container}} >/dev/null 2>&1 || true
    db_mount=()
    if [[ -n "{{db_test_data}}" ]]; then
      db_mount=(-v "{{db_test_data}}:/data/test.db:ro")
    fi
    docker run -d \
      --name {{container}} \
      --device /dev/fuse \
      --cap-add SYS_ADMIN \
      --security-opt apparmor:unconfined \
      -e GITHUB_TOKEN="$GITHUB_TOKEN" \
      -e SSH_AUTH_SOCK=/ssh-agent \
      -e GIT_SSH_COMMAND='ssh -F /dev/null -o StrictHostKeyChecking=accept-new' \
      -v "$SSH_AUTH_SOCK:/ssh-agent" \
      -v "$(pwd)/scripts/demo.sh:/tmp/demo.sh:ro" \
      "${db_mount[@]}" \
      {{image}}
    for _ in $(seq 1 60); do
      if docker exec {{container}} sh -lc "grep -qs ' /omnifs ' /proc/mounts"; then
        exit 0
      fi
      if ! docker ps --format '{{"{{.Names}}"}}' | grep -qx {{container}}; then
        docker exec {{container}} sh -lc 'cat /tmp/omnifs.log' >&2 || true
        exit 1
      fi
      sleep 1
    done
    docker exec {{container}} sh -lc 'cat /tmp/omnifs.log' >&2 || true
    exit 1

shell:
    docker exec -it {{container}} /bin/zsh

logs:
    docker exec -it {{container}} sh -lc 'cat /tmp/omnifs.log'

stop:
    docker rm -f {{container}}
