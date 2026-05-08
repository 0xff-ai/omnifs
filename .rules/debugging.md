# Debugging

**Read when:** something is failing at runtime — `Input/output error` on a
mount path, a hang on `ls`/`cd`, a clone failing silently, a FUSE op
returning the wrong thing. Read before forming a theory; user-visible probes
beat speculation.

**Update when:** discovering a new failure mode worth triaging, a new log
surface or trace channel appears, `omnifs status` grows new fields, or the
"expected noise" set changes (e.g. a previously-spurious warning becomes
load-bearing).

## Two log surfaces, not one

- `docker compose logs omnifs` — stdout/stderr from the container
  entrypoint.
- `/tmp/omnifs.log` inside the container — runtime FUSE traces and
  provider activity.

Check both when debugging. They show different things.

## Triage helpers

- `omnifs status` inside the container — fast mount/config/plugin/cache
  triage.
- `docker exec` does **not** inherit the entrypoint environment. Verify
  live runtime paths from `/proc` rather than inferring from defaults.

## Expected noise

FUSE `access(...)` warnings are expected unless they correlate with a real
failure. Don't chase them in isolation.

## When a repo path returns `Input/output error`

Check, in order:

1. `docker compose logs omnifs`
2. SSH auth inside the container (`ssh -F /dev/null -T git@github.com`)
3. Whether the mount is still present in `/proc/mounts`

## When debugging hangs or slow paths

Start with user-visible probes before theory:

1. `cd /github/<owner>`
2. `cat /dns/@google/<domain>/MX`
3. `tail -n 80 /tmp/omnifs.log`
