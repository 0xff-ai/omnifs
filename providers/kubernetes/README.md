# omnifs-provider-kubernetes

A read-only projected filesystem over a Kubernetes cluster. Resource types
(including CRDs) are discovered live from the API server, so the tree reflects
whatever the cluster actually serves.

## How it reaches the cluster

The provider talks to the API server through the configured `endpoint`, which
is turned into callout URLs by the SDK's `HttpEndpoint`. Two transports work:

- **`unix://` socket (recommended).** Point `endpoint` at a local
  `kubectl proxy --unix-socket` socket. kubectl terminates TLS and injects the
  active kubeconfig context's credentials, so the provider issues plain,
  unauthenticated HTTP over the socket and never handles a token. This works
  against **any** cluster `kubectl` can reach — client-cert (kind/minikube/
  kubeadm), EKS/GKE/AKS exec plugins, OIDC, and custom CAs are all handled
  upstream by kubectl. The host's `unix:` callout transport bypasses the
  HTTPS-only / private-IP egress restrictions that otherwise block local and
  in-cluster API servers.
- **`https://` API server.** Works only for clusters reachable with
  system-trust TLS (a public hostname + publicly-trusted cert) and a bearer
  token configured in mount auth. The host callout currently has no custom-CA
  or client-cert (mTLS) support, so self-signed clusters and private IPs are
  not reachable over this transport. Prefer the `unix://` path.

### Running with kubectl proxy

```bash
# Pick a socket path and start a read-only proxy against your current context.
kubectl proxy \
  --unix-socket=/run/omnifs/k8s.sock \
  --reject-methods='POST,PUT,PATCH,DELETE'
```

`--reject-methods` keeps the proxy read-only (defense in depth — this provider
only ever issues `GET`). `kubectl proxy` already rejects pod `exec`/`attach`.

### Mount config

```json
{
  "provider": "omnifs_provider_kubernetes.wasm",
  "mount": "k8s",
  "config": {
    "endpoint": "unix:///run/omnifs/k8s.sock",
    "hide_empty_types": false
  }
}
```

The host grants the socket automatically from `config.endpoint` (no separate
`capabilities.unix_sockets` entry needed). One mount targets one cluster/
context; to browse another cluster, add another mount pointed at a second
proxy socket.

`hide_empty_types` (default `false`): when `true`, listing a namespace or
`/cluster` shows only resource types that currently have at least one instance,
rather than the full discovery catalog (~40 namespaced types). It costs one
batched `limit=1` probe per type per listing; empty types stay directly
navigable (only `ls`/`readdir` is filtered, not `lookup`).

## Filesystem layout

```text
/namespaces/<ns>/<type>/<name>/
    manifest.yaml      # full object, managedFields stripped (kubectl-get style)
    manifest.json
    status.yaml        # the .status subobject
    events.txt         # events involving this object
/namespaces/<ns>/pods/<name>/logs/<container>.log
/cluster/<type>/<name>/
    manifest.yaml
    manifest.json
    status.yaml
```

- `/namespaces/<ns>` lists the namespaced resource types; `/cluster` lists the
  cluster-scoped types. Both are populated from API discovery, so CRDs appear
  automatically. A plural that collides across API groups is disambiguated to
  `<plural>.<group>`; built-ins keep the bare name.
- `Namespace` objects are cluster-scoped, so they live at
  `/cluster/namespaces/<ns>/...`. The top-level `/namespaces/` tree is the
  grouping for namespaced resources.

Examples:

```bash
cat /omnifs/k8s/namespaces/default/pods/web-7d9f/manifest.yaml
cat /omnifs/k8s/namespaces/default/deployments/web/status.yaml
cat /omnifs/k8s/namespaces/default/pods/web-7d9f/logs/web.log
cat /omnifs/k8s/cluster/nodes/node-1/manifest.yaml
grep -r --include=manifest.yaml image: /omnifs/k8s/namespaces/default
```

## Scope and limitations (v1)

- **Read-only.** No writes/mutations, consistent with the omnifs read model.
- **No live watch.** Each read is an on-demand fetch; listings and objects are
  re-fetched on access (the host caches bytes; `manifest`/`status` leaves are
  marked mutable so reads stay fresh). A polling-based change feed
  (periodic `LIST` keyed by `resourceVersion`, emitting cache invalidations) is
  a natural follow-up using the runtime's `refresh-interval`/`timer-tick`
  mechanism.
- **`describe.txt`** is intentionally omitted — a faithful `kubectl describe`
  renderer is large and per-kind; the raw `manifest.yaml`/`status.yaml`/
  `events.txt` cover the same information for v1.
- **Pod logs** are a current-logs snapshot per container (equivalent to
  `kubectl logs <pod> -c <container>`). Streaming follow (`tail -f`),
  `--previous`, and `--timestamps` are follow-ups (they need the ranged/volatile
  file path / extra log options).
- **Listings** issue a single unpaginated `LIST` (the API returns the full
  collection — no silent truncation). Chunked listing (`limit`/`continue`, as
  `kubectl` does at 500) is a follow-up for very large namespaces.
- **Discovery** walks `/api/v1` plus every API group once per instance and
  caches it. Each group's versions are queried preferred-first, so a
  multi-version resource resolves to its preferred version while a resource
  present only in a non-preferred version still surfaces — matching client-go's
  `ServerPreferredResources`. A group version whose discovery call fails (e.g. a
  flaky aggregated API) is skipped rather than failing the whole tree.
- **`events.txt`** filters by `involvedObject.{name,namespace,kind,uid}` — the
  same field selector `kubectl describe`/`kubectl get events` build — so events
  of a same-named object of another kind (or a prior incarnation) don't leak in.
- **`manifest.yaml`/`manifest.json`** strip only `metadata.managedFields` (as
  `kubectl get -o yaml` does by default since v1.21); the
  `last-applied-configuration` annotation is preserved, matching `kubectl get`.
