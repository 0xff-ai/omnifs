# Kubernetes dev mount

Local k3s cluster for the `default` and `full` dev profiles on Linux. `compose.yaml` brings up k3s plus a `kubectl proxy`; the proxy writes a Unix socket into `~/.omnifs-dev/fixtures/k8s/` for the host-native daemon. The mount is skipped on macOS because Docker Desktop cannot proxy that live AF_UNIX socket across its VM boundary.
