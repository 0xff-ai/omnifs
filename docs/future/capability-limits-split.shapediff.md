# Split capabilities from limits

The current provider model mixes two different things in one manifest/schema
surface:

- capabilities: authority the host may grant and enforce on provider callouts,
  such as domains, git remotes, unix sockets, and WASI preopens.
- limits: scalar ceilings the host applies to runtime resources, such as memory
  and blob byte budgets.

The intended shape makes that distinction true at every boundary: provider
authoring, embedded provider metadata, generated provider schema, mount spec
defaults, host runtime construction, CLI display, upgrade classification, tests,
and docs. The mount spec remains the runtime authority; provider manifests only
seed explicit creation-time defaults.

This contract deliberately does not claim to add new Wasmtime memory-limit
enforcement. If this slice wires `max_memory_mb` into a Wasmtime store limiter,
that must be explicit in implementation and tests. The required part here is
that memory is no longer modeled, serialized, displayed, or checked as a
capability grant.

```shapediff
version 0.4
language rust
change split-capability-limits
base b650c2f0fb525b0d13c20345ba2efb53b5cb36fc
sep ;;
groups:
  manifest-limit-split: scalar resource declarations leave provider capability needs and arrive in provider manifest limits
  spec-limit-split: scalar runtime values leave mount capability grants and arrive in mount limits
  display-limit-split: user-facing text stops calling scalar limits capabilities

@@ C1 file=crates/omnifs-caps/src/model.rs
= pub enum Need {
- MemoryMb { value: u32, why: String, dynamic: bool }   ;; {manifest-limit-split}->
- FetchBlobBytes { value: u64, why: String, dynamic: bool }   ;; {manifest-limit-split}->
- ReadBlobBytes { value: u64, why: String, dynamic: bool }   ;; {manifest-limit-split}->
- pub max_memory_mb: Option<u32>   ;; {spec-limit-split}->
- pub max_fetch_blob_bytes: Option<u64>   ;; {spec-limit-split}->
- pub max_read_blob_bytes: Option<u64>   ;; {spec-limit-split}->
+ pub struct Limit<T> { pub value: T, pub why: String }
+ pub struct LimitDeclarations { pub max_memory_mb: Option<Limit<u32>>, pub max_fetch_blob_bytes: Option<Limit<u64>>, pub max_read_blob_bytes: Option<Limit<u64>> }   ;; <-{manifest-limit-split}
+ pub struct Limits { pub max_memory_mb: Option<u32>, pub max_fetch_blob_bytes: Option<u64>, pub max_read_blob_bytes: Option<u64> }   ;; <-{spec-limit-split}
+ impl LimitDeclarations { pub fn is_empty(&self) -> bool { ... } }
+ impl Limits { pub fn from_declarations(declarations: &LimitDeclarations) -> Self { ... } }
! `Need` models only authority-bearing capability needs: `Domain`, `GitRepo`, `UnixSocket`, and `PreopenedPath`.
! `dynamic` applies only to capability needs; scalar limits must not carry `dynamic`.
! `Grants` models only grantable authorities and must not contain memory or blob byte fields after this change.   ;; stays-gone

@@ C2 file=crates/omnifs-caps/src/check.rs
= impl Grants {
- Need::MemoryMb { .. } | Need::FetchBlobBytes { .. } | Need::ReadBlobBytes { .. } => None   ;; {manifest-limit-split}->
- Need::MemoryMb { value, .. } => grants.max_memory_mb = Some(*value)   ;; {spec-limit-split}->
- Need::FetchBlobBytes { value, .. } => grants.max_fetch_blob_bytes = Some(*value)   ;; {spec-limit-split}->
- Need::ReadBlobBytes { value, .. } => grants.max_read_blob_bytes = Some(*value)   ;; {spec-limit-split}->
~ Grants::satisfies only checks capability needs against capability grants.   ;; behavior
+ Grants::from_needs lowers only capability needs into `Grants`.
+ Limits::from_declarations lowers provider limit declarations into mount/runtime `Limits`.   ;; <-{spec-limit-split}
! A scalar limit can no longer be silently skipped by the required-capabilities check, because it is no longer an input to that check.

@@ M1 file=crates/omnifs-provider/src/manifest.rs
= pub struct ProviderManifest {
= pub capabilities: Vec<Need>
+ #[serde(default, skip_serializing_if = "LimitDeclarations::is_empty")]
+ pub limits: LimitDeclarations   ;; <-{manifest-limit-split}
= pub fn provider_capabilities(&self) -> Grants { Grants::from_needs(&self.capabilities) }
+ pub fn provider_limits(&self) -> Limits { Limits::from_declarations(&self.limits) }   ;; <-{spec-limit-split}
~ ProviderManifest::validate validates `capabilities.*.why` for capability needs and `limits.*.why` for limit declarations.   ;; behavior
! Manifest JSON with `{"kind":"memoryMb"}` under `capabilities` is invalid.   ;; stays-gone
! Generated provider schema exposes top-level `limits`; scalar resource definitions must not appear in the `Need` schema.   ;; stays-gone

@@ M2 file=crates/omnifs-provider/schema/omnifs.provider.schema.json
= "properties": {
= "capabilities": { "type": "array", "items": { "$ref": "#/$defs/Need" } }
+ "limits": { "$ref": "#/$defs/LimitDeclarations" }   ;; <-{manifest-limit-split}
- "$defs.Need.oneOf includes memoryMb, fetchBlobBytes, or readBlobBytes variants"   ;; {manifest-limit-split}->
+ "$defs.LimitDeclarations" has optional `maxMemoryMb`, `maxFetchBlobBytes`, and `maxReadBlobBytes` declaration objects with `value` and `why`.
! `just schema` regenerates this file from the Rust model; do not hand-edit schema JSON as the primary fix.

@@ S1 file=crates/omnifs-sdk-macros/src/provider_macro.rs
= pub struct ProviderArgs {
= capabilities: Vec<omnifs_caps::Need>
+ limits: omnifs_caps::LimitDeclarations   ;; <-{manifest-limit-split}
- parse_capabilities accepts `memory_mb(...)` as a capability.   ;; {manifest-limit-split}->
+ parse_capabilities accepts only `domain`, `git_repo`, `unix_socket`, and `preopened_path`.
+ parse_limits accepts `memory_mb(<int>, "why")`, `fetch_blob_bytes(<int>, "why")`, and `read_blob_bytes(<int>, "why")`.   ;; <-{manifest-limit-split}
+ build_manifest_facts_from_args writes `ProviderManifest { capabilities, limits, ... }`.
! If a provider writes `memory_mb(...)` inside `capabilities(...)`, the macro error points to `limits(memory_mb(...))`.   ;; never
! The runtime `RequestedCapabilities::max_memory_mb` remains runtime-only and must not become the install-time manifest limit source.

@@ S2 file=crates/omnifs-sdk-macros/src/lib.rs
= /// - `capabilities(domain("v", "why"), git_repo("v", "why"), ...)
~ provider macro docs describe `capabilities(...)` as authority grants and `limits(...)` as scalar resource ceilings.   ;; behavior
- docs list `memory_mb(<int>, "why")` under `capabilities(...)`.   ;; stays-gone ;; {manifest-limit-split}->
+ docs list `limits(memory_mb(<int>, "why"))` as the provider authoring form.   ;; <-{manifest-limit-split}

@@ W1 file=crates/omnifs-wit/wit/provider.wit
= variant capability-need {
- memory-mb(scalar-need)   ;; {manifest-limit-split}->
- fetch-blob-bytes(scalar-need)   ;; {manifest-limit-split}->
- read-blob-bytes(scalar-need)   ;; {manifest-limit-split}->
+ record scalar-limit { amount: u64, why: string }
+ record provider-limits { max-memory-mb: option<scalar-limit>, max-fetch-blob-bytes: option<scalar-limit>, max-read-blob-bytes: option<scalar-limit> }   ;; <-{manifest-limit-split}
= record provider-manifest {
+ limits: provider-limits   ;; <-{manifest-limit-split}
! `capability-need` contains only authority-bearing capability variants.
! `requested-capabilities.max-memory-mb` is not the provider manifest `limits.max-memory-mb` source.

@@ P1 file=crates/omnifs-mount/src/mounts/mod.rs
= pub struct Spec {
= pub capabilities: Option<Grants>
+ #[serde(skip_serializing_if = "Option::is_none")]
+ pub limits: Option<Limits>   ;; <-{spec-limit-split}
= pub struct ManifestRequirements {
~ ManifestRequirements carries capability needs and config schema only; scalar limits are not requirements for `Grants::satisfies`.   ;; behavior
! Mount spec JSON uses top-level `limits` for scalar runtime values. `capabilities.max_memory_mb`, `capabilities.max_fetch_blob_bytes`, and `capabilities.max_read_blob_bytes` must not be emitted by Registry writes.   ;; stays-gone
! Do not add a read-time fallback from manifest limits into a loaded spec; init/dev creation writes explicit spec limits.

@@ P2 file=crates/omnifs-cli/src/commands/init/spec_creation.rs
= pub(super) struct CreatedMountSpec {
= pub(super) capabilities: Option<Grants>
+ pub(super) limits: Option<Limits>   ;; <-{spec-limit-split}
= impl MountSpecCreator<'_> {
+ create seeds capabilities from `manifest.provider_capabilities()` only when manifest capabilities are non-empty.
+ create seeds limits from `manifest.provider_limits()` only when manifest limits are non-empty.   ;; <-{spec-limit-split}
! Creation-time seeding remains the only provider-manifest-to-spec defaulting path for grants and limits.

@@ P3 file=crates/omnifs-cli/src/commands/init/mount_file.rs
= impl MountFile {
+ MountFile::into_spec writes `CreatedMountSpec.limits` into `Spec::limits`.   ;; <-{spec-limit-split}
! A mount file generated by `omnifs init` puts memory under `limits.max_memory_mb`, not `capabilities.max_memory_mb`.   ;; stays-gone

@@ P4 file=crates/omnifs-cli/src/dev/mod.rs
= if spec.capabilities.is_none() && !manifest.capabilities.is_empty() {
~ dev mount materialization seeds missing `Spec::capabilities` from provider capability needs only.   ;; behavior
+ if spec.limits.is_none() && !manifest.limits.is_empty() { spec.limits = Some(manifest.provider_limits()); }   ;; <-{spec-limit-split}
! Dev mounts preserve explicitly authored `limits`; generated defaults fill only absent limits.

@@ H1 file=crates/omnifs-host/src/capability.rs
= fn allowlist_from_config(
- max_memory_mb: grants.and_then(|g| g.max_memory_mb).unwrap_or(DEFAULT_MAX_MEMORY_MB)   ;; {spec-limit-split}->
~ allowlist_from_config reads `Spec::capabilities` only for access-control allowlist fields.   ;; behavior
! `Allowlist` and `CapabilityChecker` must not carry memory or blob byte limits.   ;; stays-gone

@@ H2 file=crates/omnifs-caps/src/allowlist.rs
= pub struct Allowlist {
- pub max_memory_mb: u32   ;; {spec-limit-split}->
! `Allowlist` is an access-control decision type only: domains, git repos, git enabled, and unix sockets.
! Tests construct allowlists without any scalar limit field.   ;; stays-gone

@@ H3 file=crates/omnifs-host/src/blob.rs
= impl BlobLimits {
- let caps = config.capabilities.as_ref();   ;; {spec-limit-split}->
- max_fetch_blob_bytes: caps.and_then(|c| c.max_fetch_blob_bytes)...
- max_read_blob_bytes: caps.and_then(|c| c.max_read_blob_bytes)...
+ BlobLimits::from_config reads `config.limits` and applies host defaults when limits are absent.   ;; <-{spec-limit-split}
! Blob byte limits are runtime limits, not capability grants or allowlist entries.

@@ H4 file=crates/omnifs-host/src/runtime.rs
= pub struct Runtime {
+ Runtime construction builds capability enforcement and runtime limits as separate values from the same `Spec`.   ;; <-{spec-limit-split}
~ CapabilityChecker::from_config receives only the data needed for access-control enforcement.   ;; behavior
~ BlobExecutor receives blob limits from `Spec::limits`.
! Do not describe `max_memory_mb` as enforced by the capability checker or allowlist.

@@ U1 file=crates/omnifs-mount/src/upgrade.rs
= pub enum UpgradePlan {
- CapabilityOrAuth { caps: Vec<CapabilityChange>, auth: Option<AuthDelta> }   ;; {display-limit-split}->
+ CapabilityLimitOrAuth { capabilities: Vec<CapabilityChange>, limits: Vec<LimitChange>, auth: Option<AuthDelta> }   ;; <-{display-limit-split}
- extract_capabilities includes `Need::MemoryMb`, `Need::FetchBlobBytes`, or `Need::ReadBlobBytes`.   ;; {manifest-limit-split}->
+ extract_capabilities flattens only authority-bearing `Need` variants.
+ extract_limits flattens `ProviderManifest::limits` and reports `LimitChange` entries by limit name and value.   ;; <-{manifest-limit-split}
! Limit changes still require explicit re-consent; they are just no longer reported as capability changes.

@@ L1 file=crates/omnifs-cli/src/capability.rs
= pub(crate) fn capability_label(entry: &Need) -> &'static str {
- Need::MemoryMb { .. } => "Memory limit"   ;; {display-limit-split}->
- Need::FetchBlobBytes { .. } => "Fetch body limit"   ;; {display-limit-split}->
- Need::ReadBlobBytes { .. } => "Blob read limit"   ;; {display-limit-split}->
+ capability_label handles only authority-bearing capability needs.
+ limit_label and limit_value format `LimitDeclarations` entries separately.   ;; <-{display-limit-split}
! A scalar limit must not be rendered by any function named `capability_*`.   ;; stays-gone

@@ L2 file=crates/omnifs-cli/src/commands/init/mod.rs
= pub(crate) fn print_capability_justifications(manifest: &ProviderManifest) {
- A provider memory budget prints under `Provider capabilities`.   ;; {display-limit-split}->
+ Provider capabilities prints only manifest.capabilities.
+ Provider limits prints manifest.limits when non-empty.   ;; <-{display-limit-split}
! Memory and blob budgets remain visible to users during init, but under the limits heading.

@@ L3 file=crates/omnifs-cli/src/commands/setup/mod.rs
= fn capability_summary(manifest: &ProviderManifest) -> Option<String> {
~ setup rows summarize capabilities and limits without labeling scalar limits as capabilities.   ;; behavior ;; <-{display-limit-split}
! `no extra capabilities` is still valid only when there are no capabilities; absent limits should not hide existing capability needs.

@@ L4 file=crates/omnifs-cli/src/mount_report.rs
= pub(crate) struct ProviderReadyStatus {
= pub(crate) max_memory_mb: Option<u32>
~ ProviderReadyStatus.max_memory_mb is sourced from `Spec::limits`.   ;; behavior
! Status and JSON output may continue to show max_memory, but the source is `limits`, not `capabilities`.

@@ E1 file=providers/*/src/lib.rs scope=all-matching
= #[omnifs_sdk::provider(
- capabilities(... memory_mb(...), ...)
+ capabilities(... only domain, git_repo, unix_socket, or preopened_path entries ...)
+ limits(memory_mb(...))   ;; <-{manifest-limit-split}
! Move every current provider memory declaration to `limits(...)`: arxiv, db, dns, docker, github, kubernetes, linear, oura, and test-provider.
! DB keeps `preopened_path(dynamic, ...)` in capabilities and moves only `memory_mb(...)` to limits.

@@ T1 file=crates/omnifs-provider/src/manifest.rs ;; test
= mod tests {
- invalid memory capability tests parse `memoryMb` under `capabilities`.   ;; {manifest-limit-split}->
+ manifest tests assert that scalar variants under `capabilities` are rejected.
+ manifest tests assert that `limits.maxMemoryMb.value` rejects fractional and out-of-range values.
+ schema drift tests assert `Need` has no scalar variants and top-level `limits` exists.   ;; <-{manifest-limit-split}

@@ T2 file=crates/omnifs-caps/src/check.rs ;; test
= mod tests {
+ Grants::from_needs ignores no scalar cases because `Need` has none.
+ Limits::from_declarations preserves max memory, max fetch blob bytes, and max read blob bytes.   ;; <-{spec-limit-split}
+ Grants::satisfies tests cover only authority needs: domain, git repo, unix socket, and preopened path.

@@ T3 file=crates/omnifs-cli/src/commands/init/mod.rs ;; test
= fn generate_mount_config_materializes_schema_defaults() {
- assert_eq!(capabilities.max_memory_mb, Some(128));   ;; {spec-limit-split}->
+ assert_eq!(created.limits.unwrap().max_memory_mb, Some(128));   ;; <-{spec-limit-split}
= fn mount_file_includes_generated_config_and_capabilities() {
+ mount-file tests assert generated JSON writes scalar values under top-level `limits`.
! No init test may assert `capabilities.max_memory_mb`.

@@ T4 file=crates/omnifs-host/src/blob.rs ;; test
= mod tests {
~ blob limit tests build specs with `limits`, not `capabilities`, when testing fetch/read byte ceilings.   ;; behavior ;; <-{spec-limit-split}
! Host tests constructing `Allowlist` no longer include `max_memory_mb`.

@@ D1 file=docs/contracts/10-system.md
= Providers never hold stored tokens. Provider metadata declares auth needs and capability needs.
~ docs distinguish provider capability needs from provider limit declarations.   ;; behavior
! Do not describe memory or blob byte budgets as provider authority, grants, or callout capabilities.   ;; stays-gone

@@ D2 file=docs/contracts/20-provider-sdk.md
= Provider metadata
~ provider metadata contract says manifests expose `capabilities` for authority needs and `limits` for scalar runtime ceilings.   ;; behavior ;; <-{display-limit-split}
! SDK examples must not put `memory_mb` inside `capabilities(...)`.

@@ D3 file=docs/future/provider-contract-versioning.md
= ## Required capabilities and over-grant
~ future upgrade notes keep required-capabilities discussion scoped to authority grants.   ;; behavior
+ limit-change discussion explains that scalar limit changes still require explicit re-consent without being capability changes.   ;; <-{display-limit-split}
! The phrase `Scalar resource limits ... are out of scope` must not preserve the old model where limits live inside capability needs.

@@ F1 file=* ;; forbid
! `Need::MemoryMb`, `Need::FetchBlobBytes`, and `Need::ReadBlobBytes` must not appear in after-state Rust code.   ;; never
! `Grants::max_memory_mb`, `Grants::max_fetch_blob_bytes`, and `Grants::max_read_blob_bytes` must not appear in after-state Rust code.   ;; never
! Provider source must not contain `capabilities(` with nested `memory_mb(`.   ;; never
! Checked-in provider schema must not define `memoryMb`, `fetchBlobBytes`, or `readBlobBytes` as `Need` variants.   ;; never
! User-facing headings or summaries must not call scalar runtime limits capabilities.   ;; never

@@ V1 file=just/providers.just ;; test
= just schema
+ Validation for the implementation includes `just schema`, `just providers check`, `just providers build`, and `just providers validate`.
+ Host/runtime consumers touched by the split run `just host test` or narrower crate tests that cover mount materialization, runtime limits, blob limits, and CLI status/init output.
! Generated provider schema and provider WASM metadata must be regenerated before accepting the patch.
```
