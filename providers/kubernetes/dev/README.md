# Kubernetes dev mount

Local k3s cluster for `omnifs dev --profile full`. `compose.yaml` brings up k3s plus a `kubectl proxy` on a unix socket bind-mounted into the runtime container.
