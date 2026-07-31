# Editor schema for the plain-root omnifs KCL fallback. The Rust authoring
# types remain authoritative because the in-process KCL API cannot inject this
# source as an importable package.

schema Config:
    apiVersion: str
    resources: [ProviderResource | CredentialResource | MountResource | AttachmentResource]

    check:
        apiVersion == "omnifs.dev/v1alpha1"

schema ProviderSource:
    embedded?: str
    local?: LocalProviderSource
    digest?: str

    check:
        embedded or local or digest
        not embedded or not local
        not embedded or not digest
        not local or not digest

schema LocalProviderSource:
    path: str
    expectedDigest: str

    check:
        path
        expectedDigest

schema ProviderSpec:
    name: str
    source: ProviderSource

schema CredentialSpec:
    name: str
    provider: str
    scheme: str
    account: str

schema ResourceLimits:
    maxMemoryMb?: int
    maxFetchBlobBytes?: int

schema MountSpec:
    name: str
    provider: str
    credential?: str
    config: {str:any}
    limits?: ResourceLimits

schema AttachmentSpec:
    name: str
    protocol: str
    runtime: str
    location: str
    dockerImage?: str
    libkrunGuestImage?: str

schema ProviderResource:
    kind: str
    spec: ProviderSpec

    check:
        kind == "Provider"

schema CredentialResource:
    kind: str
    spec: CredentialSpec

    check:
        kind == "Credential"

schema MountResource:
    kind: str
    spec: MountSpec

    check:
        kind == "Mount"

schema AttachmentResource:
    kind: str
    spec: AttachmentSpec

    check:
        kind == "Attachment"
