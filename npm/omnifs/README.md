# omnifs

`omnifs` is the host CLI for OmnIFS. The npm package installs a native `omnifs`
binary for your host platform; the runtime itself remains the Docker image that
`omnifs up` pulls and starts.

```bash
npm install -g @0xff-ai/omnifs
omnifs init github
omnifs up
omnifs shell
```

The npm install step does not pull the Docker image. Image selection and pull
remain owned by the Rust CLI so `omnifs up` can use the version-matched runtime
image, honor `--image` and `OMNIFS_IMAGE`, materialize credentials, and report
Docker errors in one place.

On macOS, the CLI talks to Docker Desktop and the OmnIFS mount is available
inside the Linux runtime container. It is not a native Finder or host-shell mount.

Supported npm host binaries:

- `darwin-arm64`
- `darwin-x64`
- `linux-arm64`
- `linux-x64`
