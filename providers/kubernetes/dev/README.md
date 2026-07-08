# Kubernetes dev mount

Local k3s cluster for `just dev --profile full`. `compose.yaml` brings up k3s plus a `kubectl proxy` on a unix socket bind-mounted into the runtime container. `manifests/seed.yaml` seeds a `demo` namespace with a ConfigMap and a Deployment so the projected tree has something real to browse.

## Container usage (`just dev`)

`scripts/dev.ts` starts the compose project with `OMNIFS_K8S_SOCK_DIR` pointing at a host directory, binds that directory into the omnifs runtime container at `/run/omnifs`, and the provider reads the proxy's socket through its default `unix:///run/omnifs/k8s.sock` endpoint. Both ends of the socket live inside the Docker VM, so this path only works container-to-container.

## Host usage (tape recording)

A unix socket created inside a container does not bridge back out to a macOS host through the bind mount, so host processes (the `omnifs-itest` tape recorder) cannot use the in-container proxy. Instead, the k3s service publishes the API server on `127.0.0.1:16443` and host-side tooling runs its own `kubectl proxy`:

```bash
export OMNIFS_K8S_SOCK_DIR=$(mktemp -d)   # compose requires it; unused on this path
docker compose -p <project> -f compose.yaml up -d --wait

# Extract the admin kubeconfig and point it at the published port.
docker compose -p <project> -f compose.yaml cp k3s:/output/kubeconfig.yaml <dest>/kubeconfig.yaml
kubectl --kubeconfig <dest>/kubeconfig.yaml config set-cluster default --server=https://127.0.0.1:16443

# Terminate TLS and auth on the host, re-exposing the API as plain HTTP on a
# unix socket, exactly like the in-container proxy does for `just dev`.
kubectl proxy --kubeconfig <dest>/kubeconfig.yaml --unix-socket=/tmp/omnifs-itest-k8s.sock
```

The recorder's socket path `/tmp/omnifs-itest-k8s.sock` is pinned: it is embedded (hex-encoded) in the `unix://` request URLs of the checked-in tapes under `crates/omnifs-itest/tests/kubernetes/tapes/`, so re-recording must serve the proxy at exactly that path. See the header of `crates/omnifs-itest/tests/kubernetes/scenarios.rs`.
