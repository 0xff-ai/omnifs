//! Generated control wire types and strict domain conversions.

#[allow(
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::large_enum_variant,
    clippy::too_many_lines
)]
pub mod wire {
    tonic::include_proto!("omnifs.control.v1");
}

use crate::{
    ActionKind, ActionPhase, ActionReceipt, ActiveMutation, ApplyReceipt, ApplyResourcesRequest,
    AttachmentDefinition, AttachmentProgress, AttachmentProgressStage, CONTROL_RESOURCE_MAX_COUNT,
    ControlError, ControlErrorCode, CredentialClientOverrides, CredentialDefinition,
    CredentialHealth, CredentialKey, CredentialKind, CredentialMaterial, CredentialMaterialSidecar,
    CredentialProgress, CredentialProgressStage, CredentialReceipt, CredentialStatus,
    CredentialStatusKind, CredentialSubmission, DaemonHealth, DaemonInfo, DaemonInventory,
    DaemonPhase, DaemonRecovery, DaemonStatus, HealthReport, HealthState, MountCredential,
    MountDefinition, MountField, MountHealth, MountLimits, MountOpResult, MountPatch, MountRecord,
    MountResourceDefinition, MutationOp, MutationOpResult, NormalizedResourceSet, ProgressEvent,
    ProgressEventKind, ProgressSnapshot, ProgressTarget, ProviderDefinition,
    ProviderImportDisposition, ProviderImportReceipt, ProviderMetadata,
    ProviderPreparationProgress, ProviderPreparationStage, ProviderReference, RecoveryId,
    RecoveryOffer, RepairAction, RepairDisposition, RepairReceipt, ResourceChange,
    ResourceChangeAction, ResourceDeclarations, ResourceDefinition, ResourceLimits, ResourcePhase,
    ResourcePlan, ResourceSnapshot, ResourceStatus, RevokeCredentialRequest, SecretBytes,
    ServingOutcome, ServingProgress, ServingProgressStage, SetCredentialMaterialRequest,
};
use omnifs_core::{
    ActionId, AttachmentSpec, AuthRuntimeFingerprint, CredentialGeneration, CredentialVersion,
    MountName, MountRevision, MountVersion, MutationId, ProviderId, ResourceDigest, ResourceKey,
    ResourceKind, ResourceName, ResourceRevision,
};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FromGrpcError {
    #[error("missing required field `{0}`")]
    Missing(&'static str),
    #[error("invalid {kind} length: expected {expected}, got {actual}")]
    Length {
        kind: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("invalid {0} value")]
    Invalid(&'static str),
    #[error("unspecified {0} enum")]
    Unspecified(&'static str),
    #[error("zero {0} is not allowed")]
    Zero(&'static str),
    #[error("invalid path bytes in {0}")]
    Path(&'static str),
    #[error("invalid JSON in mount config: {0}")]
    Json(String),
    #[error("too many resource declarations: maximum {max}, got {count}")]
    TooManyResources { count: usize, max: usize },
    #[error("unsupported resource apiVersion `{0}`")]
    UnsupportedApiVersion(String),
}

fn req<T>(value: Option<T>, name: &'static str) -> Result<T, FromGrpcError> {
    value.ok_or(FromGrpcError::Missing(name))
}
fn exact<const N: usize>(bytes: &[u8], kind: &'static str) -> Result<[u8; N], FromGrpcError> {
    bytes.try_into().map_err(|_| FromGrpcError::Length {
        kind,
        expected: N,
        actual: bytes.len(),
    })
}
fn mutation_id(v: &[u8]) -> Result<MutationId, FromGrpcError> {
    Ok(MutationId::from_bytes(exact(v, "mutation id")?))
}
fn provider_id(v: &[u8]) -> Result<ProviderId, FromGrpcError> {
    Ok(ProviderId::from_digest(exact(v, "provider id")?))
}
fn mount_version(v: &[u8]) -> Result<MountVersion, FromGrpcError> {
    Ok(MountVersion::from_digest(exact(v, "mount version")?))
}
fn auth_fingerprint(v: &[u8]) -> Result<AuthRuntimeFingerprint, FromGrpcError> {
    Ok(AuthRuntimeFingerprint::from_digest(exact(
        v,
        "auth fingerprint",
    )?))
}
fn recovery_id(v: &[u8]) -> Result<RecoveryId, FromGrpcError> {
    Ok(RecoveryId::from_bytes(exact(v, "recovery id")?))
}
fn nz(v: u64, name: &'static str) -> Result<std::num::NonZeroU64, FromGrpcError> {
    std::num::NonZeroU64::new(v).ok_or(FromGrpcError::Zero(name))
}
#[allow(clippy::unnecessary_wraps)]
fn path_from_bytes(v: &[u8], name: &'static str) -> Result<PathBuf, FromGrpcError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let _ = name;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(v.to_vec())))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(v.to_vec())
            .map(PathBuf::from)
            .map_err(|_| FromGrpcError::Path(name))
    }
}
fn path_bytes(v: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        v.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        v.to_string_lossy().as_bytes().to_vec()
    }
}

fn daemon_phase(v: wire::DaemonPhase) -> Result<DaemonPhase, FromGrpcError> {
    match v {
        wire::DaemonPhase::DaemonStarting => Ok(DaemonPhase::Starting),
        wire::DaemonPhase::DaemonReady => Ok(DaemonPhase::Ready),
        wire::DaemonPhase::DaemonRecoveryRequired => Ok(DaemonPhase::RecoveryRequired),
        wire::DaemonPhase::Unspecified => Err(FromGrpcError::Unspecified("daemon phase")),
    }
}
fn to_daemon_phase(v: DaemonPhase) -> i32 {
    match v {
        DaemonPhase::Starting => wire::DaemonPhase::DaemonStarting as i32,
        DaemonPhase::Ready => wire::DaemonPhase::DaemonReady as i32,
        DaemonPhase::RecoveryRequired => wire::DaemonPhase::DaemonRecoveryRequired as i32,
    }
}
fn health_state(v: wire::HealthState) -> Result<HealthState, FromGrpcError> {
    match v {
        wire::HealthState::HealthStarting => Ok(HealthState::Starting),
        wire::HealthState::HealthHealthy => Ok(HealthState::Healthy),
        wire::HealthState::HealthDegraded => Ok(HealthState::Degraded),
        wire::HealthState::HealthUnhealthy => Ok(HealthState::Unhealthy),
        wire::HealthState::Unspecified => Err(FromGrpcError::Unspecified("health state")),
    }
}
fn to_health_state(v: HealthState) -> i32 {
    match v {
        HealthState::Starting => wire::HealthState::HealthStarting as i32,
        HealthState::Healthy => wire::HealthState::HealthHealthy as i32,
        HealthState::Degraded => wire::HealthState::HealthDegraded as i32,
        HealthState::Unhealthy => wire::HealthState::HealthUnhealthy as i32,
    }
}

fn health_report(v: &wire::HealthReport) -> Result<HealthReport, FromGrpcError> {
    Ok(HealthReport::new(
        health_state(
            wire::HealthState::try_from(v.state)
                .map_err(|_| FromGrpcError::Invalid("health state"))?,
        )?,
        v.message.clone(),
    ))
}
fn to_health_report(v: &HealthReport) -> wire::HealthReport {
    wire::HealthReport {
        state: to_health_state(v.state),
        message: v.message.clone(),
    }
}
fn daemon_health(v: &wire::DaemonHealth) -> Result<DaemonHealth, FromGrpcError> {
    Ok(DaemonHealth::new(
        health_report(&req(v.control.clone(), "control health")?)?,
        health_report(&req(v.filesystems.clone(), "filesystem health")?)?,
        health_report(&req(v.mounts.clone(), "mount health")?)?,
    ))
}
fn to_daemon_health(v: &DaemonHealth) -> wire::DaemonHealth {
    wire::DaemonHealth {
        control: Some(to_health_report(&v.control)),
        filesystems: Some(to_health_report(&v.filesystems)),
        mounts: Some(to_health_report(&v.mounts)),
    }
}

pub fn daemon_info(v: &wire::DaemonInfo) -> Result<DaemonInfo, FromGrpcError> {
    Ok(DaemonInfo {
        version: v.version.clone(),
        pid: v.pid,
        instance_id: v.instance_id.clone(),
        executable: path_from_bytes(&v.executable, "executable")?,
        attach_unix: v
            .attach_unix
            .as_ref()
            .map(|x| path_from_bytes(x, "attach unix"))
            .transpose()?,
        attach_tcp: v
            .attach_tcp
            .as_deref()
            .map(|x| x.parse().map_err(|_| FromGrpcError::Invalid("attach tcp")))
            .transpose()?,
    })
}
pub fn to_daemon_info(v: &DaemonInfo) -> wire::DaemonInfo {
    wire::DaemonInfo {
        version: v.version.clone(),
        pid: v.pid,
        instance_id: v.instance_id.clone(),
        executable: path_bytes(&v.executable).into(),
        attach_unix: v.attach_unix.as_ref().map(|x| path_bytes(x).into()),
        attach_tcp: v.attach_tcp.map(|x| x.to_string()),
    }
}
fn recovery_offer(v: &wire::RecoveryOffer) -> Result<RecoveryOffer, FromGrpcError> {
    Ok(RecoveryOffer {
        id: recovery_id(&v.id)?,
        actions: v
            .actions
            .iter()
            .map(|x| {
                match wire::RepairAction::try_from(*x)
                    .map_err(|_| FromGrpcError::Invalid("repair action"))?
                {
                    wire::RepairAction::RepairRecreateControlStore => {
                        Ok(RepairAction::RecreateControlStore)
                    },
                    wire::RepairAction::Unspecified => {
                        Err(FromGrpcError::Unspecified("repair action"))
                    },
                }
            })
            .collect::<Result<_, _>>()?,
    })
}
fn to_recovery_offer(v: &RecoveryOffer) -> wire::RecoveryOffer {
    wire::RecoveryOffer {
        id: v.id.as_bytes().to_vec().into(),
        actions: v
            .actions
            .iter()
            .map(|x| match x {
                RepairAction::RecreateControlStore => {
                    wire::RepairAction::RepairRecreateControlStore as i32
                },
            })
            .collect(),
    }
}
pub fn repair_receipt(v: &wire::RepairReceipt) -> Result<RepairReceipt, FromGrpcError> {
    let action = match wire::RepairAction::try_from(v.action)
        .map_err(|_| FromGrpcError::Invalid("repair action"))?
    {
        wire::RepairAction::RepairRecreateControlStore => RepairAction::RecreateControlStore,
        wire::RepairAction::Unspecified => return Err(FromGrpcError::Unspecified("repair action")),
    };
    let disposition = match wire::RepairDisposition::try_from(v.disposition)
        .map_err(|_| FromGrpcError::Invalid("repair disposition"))?
    {
        wire::RepairDisposition::RepairFreshStoreCreated => RepairDisposition::FreshStoreCreated,
        wire::RepairDisposition::RepairCorruptStoreArchived => {
            RepairDisposition::CorruptStoreArchived
        },
        wire::RepairDisposition::Unspecified => {
            return Err(FromGrpcError::Unspecified("repair disposition"));
        },
    };
    Ok(RepairReceipt {
        instance_id: v.instance_id.clone(),
        recovery_id: recovery_id(&v.recovery_id)?,
        action,
        disposition,
    })
}
pub fn to_repair_receipt(v: &RepairReceipt) -> wire::RepairReceipt {
    wire::RepairReceipt {
        instance_id: v.instance_id.clone(),
        recovery_id: v.recovery_id.as_bytes().to_vec().into(),
        action: wire::RepairAction::RepairRecreateControlStore as i32,
        disposition: match v.disposition {
            RepairDisposition::FreshStoreCreated => {
                wire::RepairDisposition::RepairFreshStoreCreated as i32
            },
            RepairDisposition::CorruptStoreArchived => {
                wire::RepairDisposition::RepairCorruptStoreArchived as i32
            },
        },
    }
}
pub fn daemon_recovery(v: &wire::DaemonRecovery) -> Result<DaemonRecovery, FromGrpcError> {
    Ok(DaemonRecovery {
        phase: daemon_phase(
            wire::DaemonPhase::try_from(v.phase)
                .map_err(|_| FromGrpcError::Invalid("daemon phase"))?,
        )?,
        durable_revision: v.durable_revision.map(MountRevision::new),
        serving_revision: v.serving_revision.map(MountRevision::new),
        failed_mutation: v
            .failed_mutation
            .as_ref()
            .map(|x| mutation_id(x))
            .transpose()?,
        store_health: health_report(&req(v.store_health.clone(), "store health")?)?,
        repair: v.repair.as_ref().map(recovery_offer).transpose()?,
    })
}
pub fn to_daemon_recovery(v: &DaemonRecovery) -> wire::DaemonRecovery {
    wire::DaemonRecovery {
        phase: to_daemon_phase(v.phase),
        durable_revision: v.durable_revision.map(MountRevision::get),
        serving_revision: v.serving_revision.map(MountRevision::get),
        failed_mutation: v.failed_mutation.map(|x| x.as_bytes().to_vec().into()),
        store_health: Some(to_health_report(&v.store_health)),
        repair: v.repair.as_ref().map(to_recovery_offer),
    }
}

fn mount_health(v: &wire::MountHealth) -> Result<MountHealth, FromGrpcError> {
    match req(v.value.clone(), "mount health")? {
        wire::mount_health::Value::Active(_) => Ok(MountHealth::Active),
        wire::mount_health::Value::AuthRequired(_) => Ok(MountHealth::AuthRequired),
        wire::mount_health::Value::ProviderUnavailable(x) => {
            Ok(MountHealth::ProviderUnavailable { reason: x })
        },
        wire::mount_health::Value::Failed(x) => Ok(MountHealth::Failed { reason: x }),
    }
}
fn to_mount_health(v: &MountHealth) -> wire::MountHealth {
    wire::MountHealth {
        value: Some(match v {
            MountHealth::Active => wire::mount_health::Value::Active(wire::Empty {}),
            MountHealth::AuthRequired => wire::mount_health::Value::AuthRequired(wire::Empty {}),
            MountHealth::ProviderUnavailable { reason } => {
                wire::mount_health::Value::ProviderUnavailable(reason.clone())
            },
            MountHealth::Failed { reason } => wire::mount_health::Value::Failed(reason.clone()),
        }),
    }
}
pub fn mount_record(v: &wire::MountRecord) -> Result<MountRecord, FromGrpcError> {
    Ok(MountRecord {
        definition: mount_definition(&req(v.definition.clone(), "mount definition")?)?,
        provider: provider_reference(&req(v.provider.clone(), "mount provider")?)?,
        version: mount_version(&v.version)?,
        revision: MountRevision::new(v.revision),
        health: mount_health(&req(v.health.clone(), "mount health")?)?,
        auth_health: v
            .auth_health
            .map(|x| {
                credential_health(
                    wire::CredentialHealth::try_from(x)
                        .map_err(|_| FromGrpcError::Invalid("credential health"))?,
                )
            })
            .transpose()?,
        last_mutation_id: mutation_id(&v.last_mutation_id)?,
    })
}
fn credential_health(v: wire::CredentialHealth) -> Result<CredentialHealth, FromGrpcError> {
    match v {
        wire::CredentialHealth::CredentialReady => Ok(CredentialHealth::Ready),
        wire::CredentialHealth::CredentialExpiringSoon => Ok(CredentialHealth::ExpiringSoon),
        wire::CredentialHealth::CredentialExpired => Ok(CredentialHealth::Expired),
        wire::CredentialHealth::CredentialRefreshFailed => Ok(CredentialHealth::RefreshFailed),
        wire::CredentialHealth::CredentialNeedsConsent => Ok(CredentialHealth::NeedsConsent),
        wire::CredentialHealth::CredentialMissing => Ok(CredentialHealth::Missing),
        wire::CredentialHealth::CredentialStaticUnvalidated => {
            Ok(CredentialHealth::StaticUnvalidated)
        },
        wire::CredentialHealth::Unspecified => Err(FromGrpcError::Unspecified("credential health")),
    }
}
fn to_credential_health(v: CredentialHealth) -> i32 {
    match v {
        CredentialHealth::Ready => wire::CredentialHealth::CredentialReady as i32,
        CredentialHealth::ExpiringSoon => wire::CredentialHealth::CredentialExpiringSoon as i32,
        CredentialHealth::Expired => wire::CredentialHealth::CredentialExpired as i32,
        CredentialHealth::RefreshFailed => wire::CredentialHealth::CredentialRefreshFailed as i32,
        CredentialHealth::NeedsConsent => wire::CredentialHealth::CredentialNeedsConsent as i32,
        CredentialHealth::Missing => wire::CredentialHealth::CredentialMissing as i32,
        CredentialHealth::StaticUnvalidated => {
            wire::CredentialHealth::CredentialStaticUnvalidated as i32
        },
    }
}
pub fn to_mount_record(v: &MountRecord) -> wire::MountRecord {
    wire::MountRecord {
        definition: Some(to_mount_definition(&v.definition)),
        provider: Some(to_provider_reference(&v.provider)),
        version: v.version.as_bytes().to_vec().into(),
        revision: v.revision.get(),
        health: Some(to_mount_health(&v.health)),
        auth_health: v.auth_health.map(to_credential_health),
        last_mutation_id: v.last_mutation_id.as_bytes().to_vec().into(),
    }
}
pub fn to_mount_op_result(v: &MountOpResult) -> wire::MountOpResult {
    wire::MountOpResult {
        name: v.name.to_string(),
        version: v.version.map(|x| x.as_bytes().to_vec().into()),
        revision: v.revision.get(),
    }
}
pub fn mount_op_result(v: &wire::MountOpResult) -> Result<MountOpResult, FromGrpcError> {
    Ok(MountOpResult {
        name: MountName::new(v.name.clone()).map_err(|_| FromGrpcError::Invalid("mount name"))?,
        version: v.version.as_ref().map(|x| mount_version(x)).transpose()?,
        revision: MountRevision::new(v.revision),
    })
}

fn mount_info(v: &wire::MountInfo) -> Result<crate::MountInfo, FromGrpcError> {
    Ok(crate::MountInfo {
        mount: v.mount.clone(),
        provider_name: v.provider_name.clone(),
        provider_id: v.provider_id.clone(),
        auth_health: v
            .auth_health
            .map(|x| {
                credential_health(
                    wire::CredentialHealth::try_from(x)
                        .map_err(|_| FromGrpcError::Invalid("credential health"))?,
                )
            })
            .transpose()?,
    })
}

fn active_mutation(v: &wire::ActiveMutation) -> Result<ActiveMutation, FromGrpcError> {
    Ok(ActiveMutation {
        mutation_id: mutation_id(&v.mutation_id)?,
        lease_deadline_unix_ms: v.lease_deadline_unix_ms,
    })
}
fn to_active_mutation(v: &ActiveMutation) -> wire::ActiveMutation {
    wire::ActiveMutation {
        mutation_id: v.mutation_id.as_bytes().to_vec().into(),
        lease_deadline_unix_ms: v.lease_deadline_unix_ms,
    }
}

pub fn daemon_status(v: &wire::DaemonStatus) -> Result<DaemonStatus, FromGrpcError> {
    Ok(DaemonStatus {
        version: v.version.clone(),
        pid: v.pid,
        instance_id: v.instance_id.clone(),
        executable: path_from_bytes(&v.executable, "executable")?,
        attach_tcp: v
            .attach_tcp
            .as_deref()
            .map(|x| x.parse().map_err(|_| FromGrpcError::Invalid("attach tcp")))
            .transpose()?,
        filesystems: v
            .filesystems
            .iter()
            .map(filesystem_spec)
            .collect::<Result<_, _>>()?,
        mounts: v.mounts.iter().map(mount_info).collect::<Result<_, _>>()?,
        health: Box::new(daemon_health(&req(v.health.clone(), "daemon health")?)?),
        active_mutation: v
            .active_mutation
            .as_ref()
            .map(active_mutation)
            .transpose()?,
    })
}

pub fn daemon_inventory(v: &wire::DaemonInventory) -> Result<DaemonInventory, FromGrpcError> {
    Ok(DaemonInventory {
        info: daemon_info(&req(v.info.clone(), "daemon info")?)?,
        phase: daemon_phase(
            wire::DaemonPhase::try_from(v.phase)
                .map_err(|_| FromGrpcError::Invalid("daemon phase"))?,
        )?,
        durable_revision: v.durable_revision.map(MountRevision::new),
        serving_revision: v.serving_revision.map(MountRevision::new),
        health: daemon_health(&req(v.health.clone(), "daemon health")?)?,
        mounts: v
            .mounts
            .iter()
            .map(mount_record)
            .collect::<Result<_, _>>()?,
        credentials: v
            .credentials
            .iter()
            .map(credential_status)
            .collect::<Result<_, _>>()?,
        attachments: v
            .attachments
            .iter()
            .map(filesystem_spec)
            .collect::<Result<_, _>>()?,
        active_mutation: v
            .active_mutation
            .as_ref()
            .map(active_mutation)
            .transpose()?,
    })
}

pub fn to_daemon_status(v: &DaemonStatus) -> wire::DaemonStatus {
    wire::DaemonStatus {
        version: v.version.clone(),
        pid: v.pid,
        instance_id: v.instance_id.clone(),
        executable: path_bytes(&v.executable).into(),
        attach_tcp: v.attach_tcp.map(|x| x.to_string()),
        filesystems: v.filesystems.iter().map(to_filesystem_spec).collect(),
        mounts: v
            .mounts
            .iter()
            .map(|x| wire::MountInfo {
                mount: x.mount.clone(),
                provider_name: x.provider_name.clone(),
                provider_id: x.provider_id.clone(),
                auth_health: x.auth_health.map(to_credential_health),
            })
            .collect(),
        health: Some(to_daemon_health(&v.health)),
        active_mutation: v.active_mutation.as_ref().map(to_active_mutation),
    }
}
pub fn to_daemon_inventory(v: &DaemonInventory) -> wire::DaemonInventory {
    wire::DaemonInventory {
        info: Some(to_daemon_info(&v.info)),
        phase: to_daemon_phase(v.phase),
        durable_revision: v.durable_revision.map(MountRevision::get),
        serving_revision: v.serving_revision.map(MountRevision::get),
        health: Some(to_daemon_health(&v.health)),
        mounts: v.mounts.iter().map(to_mount_record).collect(),
        credentials: v.credentials.iter().map(to_credential_status).collect(),
        attachments: v.attachments.iter().map(to_filesystem_spec).collect(),
        active_mutation: v.active_mutation.as_ref().map(to_active_mutation),
    }
}

pub fn error_detail(v: &wire::ErrorDetail) -> Result<ControlError, FromGrpcError> {
    let code = match wire::ErrorCode::try_from(v.code)
        .map_err(|_| FromGrpcError::Invalid("error code"))?
    {
        wire::ErrorCode::Busy => ControlErrorCode::Busy,
        wire::ErrorCode::NotReady => ControlErrorCode::NotReady,
        wire::ErrorCode::ErrorRecoveryRequired => ControlErrorCode::RecoveryRequired,
        wire::ErrorCode::InvalidRequest => ControlErrorCode::InvalidRequest,
        wire::ErrorCode::MutationInProgress => ControlErrorCode::MutationInProgress,
        wire::ErrorCode::LeaseExpired => ControlErrorCode::LeaseExpired,
        wire::ErrorCode::LeaseNotHeld => ControlErrorCode::LeaseNotHeld,
        wire::ErrorCode::NotFound => ControlErrorCode::NotFound,
        wire::ErrorCode::AlreadyExists => ControlErrorCode::AlreadyExists,
        wire::ErrorCode::Conflict => ControlErrorCode::Conflict,
        wire::ErrorCode::UnsupportedApiVersion => ControlErrorCode::UnsupportedApiVersion,
        wire::ErrorCode::InvalidResource => ControlErrorCode::InvalidResource,
        wire::ErrorCode::StaleBaseRevision => ControlErrorCode::StaleBaseRevision,
        wire::ErrorCode::DesiredDigestMismatch => ControlErrorCode::DesiredDigestMismatch,
        wire::ErrorCode::MutationIdReuseMismatch => ControlErrorCode::MutationIdReuseMismatch,
        wire::ErrorCode::MissingProviderArtifact => ControlErrorCode::MissingProviderArtifact,
        wire::ErrorCode::PlanTooLarge => ControlErrorCode::PlanTooLarge,
        wire::ErrorCode::ActionUnavailable => ControlErrorCode::ActionUnavailable,
        wire::ErrorCode::ActionIdReuseMismatch => ControlErrorCode::ActionIdReuseMismatch,
        wire::ErrorCode::Internal => ControlErrorCode::Internal,
        wire::ErrorCode::Unspecified => return Err(FromGrpcError::Unspecified("error code")),
    };
    Ok(ControlError::new(code, v.message.clone()))
}
pub fn to_error_detail(v: &ControlError) -> wire::ErrorDetail {
    wire::ErrorDetail {
        code: match v.code {
            ControlErrorCode::Busy => wire::ErrorCode::Busy as i32,
            ControlErrorCode::NotReady => wire::ErrorCode::NotReady as i32,
            ControlErrorCode::RecoveryRequired => wire::ErrorCode::ErrorRecoveryRequired as i32,
            ControlErrorCode::InvalidRequest => wire::ErrorCode::InvalidRequest as i32,
            ControlErrorCode::MutationInProgress => wire::ErrorCode::MutationInProgress as i32,
            ControlErrorCode::LeaseExpired => wire::ErrorCode::LeaseExpired as i32,
            ControlErrorCode::LeaseNotHeld => wire::ErrorCode::LeaseNotHeld as i32,
            ControlErrorCode::NotFound => wire::ErrorCode::NotFound as i32,
            ControlErrorCode::AlreadyExists => wire::ErrorCode::AlreadyExists as i32,
            ControlErrorCode::Conflict => wire::ErrorCode::Conflict as i32,
            ControlErrorCode::UnsupportedApiVersion => {
                wire::ErrorCode::UnsupportedApiVersion as i32
            },
            ControlErrorCode::InvalidResource => wire::ErrorCode::InvalidResource as i32,
            ControlErrorCode::StaleBaseRevision => wire::ErrorCode::StaleBaseRevision as i32,
            ControlErrorCode::DesiredDigestMismatch => {
                wire::ErrorCode::DesiredDigestMismatch as i32
            },
            ControlErrorCode::MutationIdReuseMismatch => {
                wire::ErrorCode::MutationIdReuseMismatch as i32
            },
            ControlErrorCode::MissingProviderArtifact => {
                wire::ErrorCode::MissingProviderArtifact as i32
            },
            ControlErrorCode::PlanTooLarge => wire::ErrorCode::PlanTooLarge as i32,
            ControlErrorCode::ActionUnavailable => wire::ErrorCode::ActionUnavailable as i32,
            ControlErrorCode::ActionIdReuseMismatch => {
                wire::ErrorCode::ActionIdReuseMismatch as i32
            },
            ControlErrorCode::Internal => wire::ErrorCode::Internal as i32,
        },
        message: v.message.clone(),
    }
}

pub fn to_mutation_op(v: &MutationOp) -> wire::MutationOp {
    let op = match v {
        MutationOp::CreateMount(definition) => {
            wire::mutation_op::Op::CreateMount(wire::CreateMountOp {
                definition: Some(to_mount_definition(definition)),
            })
        },
        MutationOp::UpdateMount { name, patch } => {
            wire::mutation_op::Op::UpdateMount(wire::UpdateMountOp {
                name: name.to_string(),
                patch: Some(to_mount_patch(patch)),
            })
        },
        MutationOp::RemoveMount { name } => {
            wire::mutation_op::Op::RemoveMount(wire::RemoveMountOp {
                name: name.to_string(),
            })
        },
        MutationOp::SubmitCredential(submission) => {
            wire::mutation_op::Op::SubmitCredential(wire::SubmitCredentialOp {
                submission: Some(to_credential_submission(submission)),
            })
        },
        MutationOp::DeleteCredential(key) => {
            wire::mutation_op::Op::DeleteCredential(wire::DeleteCredentialOp {
                key: Some(to_credential_key(key)),
            })
        },
        MutationOp::RevokeCredential(key) => {
            wire::mutation_op::Op::RevokeCredential(wire::RevokeCredentialOp {
                key: Some(to_credential_key(key)),
            })
        },
    };
    wire::MutationOp { op: Some(op) }
}

pub fn mutation_op(v: &wire::MutationOp) -> Result<MutationOp, FromGrpcError> {
    match req(v.op.clone(), "mutation op")? {
        wire::mutation_op::Op::CreateMount(x) => Ok(MutationOp::CreateMount(mount_definition(
            &req(x.definition, "mount definition")?,
        )?)),
        wire::mutation_op::Op::UpdateMount(x) => Ok(MutationOp::UpdateMount {
            name: MountName::new(x.name).map_err(|_| FromGrpcError::Invalid("mount name"))?,
            patch: mount_patch(&req(x.patch, "mount patch")?)?,
        }),
        wire::mutation_op::Op::RemoveMount(x) => Ok(MutationOp::RemoveMount {
            name: MountName::new(x.name).map_err(|_| FromGrpcError::Invalid("mount name"))?,
        }),
        wire::mutation_op::Op::SubmitCredential(x) => Ok(MutationOp::SubmitCredential(
            credential_submission(&req(x.submission, "credential submission")?)?,
        )),
        wire::mutation_op::Op::DeleteCredential(x) => Ok(MutationOp::DeleteCredential(
            credential_key(&req(x.key, "credential key")?),
        )),
        wire::mutation_op::Op::RevokeCredential(x) => Ok(MutationOp::RevokeCredential(
            credential_key(&req(x.key, "credential key")?),
        )),
    }
}

pub fn to_mutation_op_result(v: &MutationOpResult) -> wire::MutationOpResult {
    let result = match v {
        MutationOpResult::Mount(x) => {
            wire::mutation_op_result::Result::Mount(to_mount_op_result(x))
        },
        MutationOpResult::Credential(x) => {
            wire::mutation_op_result::Result::Credential(to_credential_status(x))
        },
    };
    wire::MutationOpResult {
        result: Some(result),
    }
}
pub fn mutation_op_result(v: &wire::MutationOpResult) -> Result<MutationOpResult, FromGrpcError> {
    match req(v.result.clone(), "mutation op result")? {
        wire::mutation_op_result::Result::Mount(x) => {
            Ok(MutationOpResult::Mount(mount_op_result(&x)?))
        },
        wire::mutation_op_result::Result::Credential(x) => {
            Ok(MutationOpResult::Credential(credential_status(&x)?))
        },
    }
}

pub fn to_serving_outcome(v: &ServingOutcome) -> wire::ServingOutcome {
    wire::ServingOutcome {
        serving: v.serving,
        recovery_detail: v.recovery_detail.clone(),
    }
}
pub fn serving_outcome(v: &wire::ServingOutcome) -> ServingOutcome {
    ServingOutcome {
        serving: v.serving,
        recovery_detail: v.recovery_detail.clone(),
    }
}

fn provider_reference(v: &wire::ProviderReference) -> Result<ProviderReference, FromGrpcError> {
    Ok(ProviderReference {
        id: provider_id(&v.id)?,
        name: v.name.clone(),
        version: v.version.clone(),
    })
}
fn to_provider_reference(v: &ProviderReference) -> wire::ProviderReference {
    wire::ProviderReference {
        id: v.id.as_bytes().to_vec().into(),
        name: v.name.clone(),
        version: v.version.clone(),
    }
}
pub fn provider_metadata(v: &wire::ProviderMetadata) -> Result<ProviderMetadata, FromGrpcError> {
    let reference = req(v.reference.clone(), "provider reference")?;
    Ok(ProviderMetadata {
        reference: provider_reference(&reference)?,
        manifest: v.manifest.to_vec(),
    })
}
pub fn to_provider_metadata(v: &ProviderMetadata) -> wire::ProviderMetadata {
    wire::ProviderMetadata {
        reference: Some(to_provider_reference(&v.reference)),
        manifest: v.manifest.clone().into(),
    }
}

pub fn provider_entry(
    v: &wire::ProviderEntry,
) -> Result<(ProviderMetadata, bool, bool), FromGrpcError> {
    if !v.embedded && !v.retained {
        return Err(FromGrpcError::Invalid("provider availability"));
    }
    Ok((
        provider_metadata(&req(v.metadata.clone(), "provider metadata")?)?,
        v.embedded,
        v.retained,
    ))
}

pub fn to_provider_upload_start(
    file_name: &str,
    total_length: u64,
    digest: &ProviderId,
) -> wire::ProviderUploadStart {
    wire::ProviderUploadStart {
        file_name: file_name.to_owned(),
        total_length,
        digest: digest.as_bytes().to_vec().into(),
    }
}

pub fn provider_import_receipt(
    v: &wire::ProviderImportReceipt,
) -> Result<ProviderImportReceipt, FromGrpcError> {
    let disposition = match wire::ProviderImportDisposition::try_from(v.disposition)
        .map_err(|_| FromGrpcError::Invalid("provider import disposition"))?
    {
        wire::ProviderImportDisposition::ProviderImportInserted => {
            ProviderImportDisposition::Inserted
        },
        wire::ProviderImportDisposition::ProviderImportUnchanged => {
            ProviderImportDisposition::Unchanged
        },
        wire::ProviderImportDisposition::ProviderImportRepaired => {
            ProviderImportDisposition::Repaired
        },
        wire::ProviderImportDisposition::Unspecified => {
            return Err(FromGrpcError::Unspecified("provider import disposition"));
        },
    };
    Ok(ProviderImportReceipt {
        provider: provider_reference(&req(v.provider.clone(), "provider reference")?)?,
        disposition,
    })
}

pub fn to_provider_import_receipt(v: &ProviderImportReceipt) -> wire::ProviderImportReceipt {
    wire::ProviderImportReceipt {
        provider: Some(to_provider_reference(&v.provider)),
        disposition: match v.disposition {
            ProviderImportDisposition::Inserted => {
                wire::ProviderImportDisposition::ProviderImportInserted as i32
            },
            ProviderImportDisposition::Unchanged => {
                wire::ProviderImportDisposition::ProviderImportUnchanged as i32
            },
            ProviderImportDisposition::Repaired => {
                wire::ProviderImportDisposition::ProviderImportRepaired as i32
            },
        },
    }
}

pub fn mount_definition(v: &wire::MountDefinition) -> Result<MountDefinition, FromGrpcError> {
    let name = MountName::new(v.name.clone()).map_err(|_| FromGrpcError::Invalid("mount name"))?;
    serde_json::from_slice::<serde_json::Value>(&v.config)
        .map_err(|e| FromGrpcError::Json(e.to_string()))?;
    Ok(MountDefinition {
        name,
        provider: provider_id(&v.provider)?,
        auth: v.auth.as_ref().map(|x| MountCredential {
            scheme: x.scheme.clone(),
            account_label: x.account_label.clone(),
        }),
        limits: v.limits.as_ref().map(|x| MountLimits {
            max_memory_mb: x.max_memory_mb,
            max_fetch_blob_bytes: x.max_fetch_blob_bytes,
        }),
        config: v.config.to_vec(),
    })
}
pub fn to_mount_definition(v: &MountDefinition) -> wire::MountDefinition {
    wire::MountDefinition {
        name: v.name.to_string(),
        provider: v.provider.as_bytes().to_vec().into(),
        auth: v.auth.as_ref().map(|x| wire::MountCredential {
            scheme: x.scheme.clone(),
            account_label: x.account_label.clone(),
        }),
        limits: v.limits.as_ref().map(|x| wire::MountLimits {
            max_memory_mb: x.max_memory_mb,
            max_fetch_blob_bytes: x.max_fetch_blob_bytes,
        }),
        config: v.config.clone().into(),
    }
}

fn auth_patch(
    v: Option<&wire::MountCredentialPatch>,
) -> Result<MountField<MountCredential>, FromGrpcError> {
    let Some(v) = v else {
        return Ok(MountField::Keep);
    };
    match req(v.value.clone(), "auth patch")? {
        wire::mount_credential_patch::Value::Keep(_) => Ok(MountField::Keep),
        wire::mount_credential_patch::Value::Set(x) => Ok(MountField::Set(MountCredential {
            scheme: x.scheme,
            account_label: x.account_label,
        })),
        wire::mount_credential_patch::Value::Clear(_) => Ok(MountField::Clear),
    }
}
fn limits_patch(
    v: Option<&wire::MountLimitsPatch>,
) -> Result<MountField<MountLimits>, FromGrpcError> {
    let Some(v) = v else {
        return Ok(MountField::Keep);
    };
    match req(v.value, "limits patch")? {
        wire::mount_limits_patch::Value::Keep(_) => Ok(MountField::Keep),
        wire::mount_limits_patch::Value::Set(x) => Ok(MountField::Set(MountLimits {
            max_memory_mb: x.max_memory_mb,
            max_fetch_blob_bytes: x.max_fetch_blob_bytes,
        })),
        wire::mount_limits_patch::Value::Clear(_) => Ok(MountField::Clear),
    }
}
fn bytes_patch(v: Option<&wire::BytesPatch>) -> Result<MountField<Vec<u8>>, FromGrpcError> {
    let Some(v) = v else {
        return Ok(MountField::Keep);
    };
    match req(v.value.clone(), "config patch")? {
        wire::bytes_patch::Value::Keep(_) => Ok(MountField::Keep),
        wire::bytes_patch::Value::Set(x) => {
            serde_json::from_slice::<serde_json::Value>(&x)
                .map_err(|e| FromGrpcError::Json(e.to_string()))?;
            Ok(MountField::Set(x.to_vec()))
        },
        wire::bytes_patch::Value::Clear(_) => Ok(MountField::Clear),
    }
}
pub fn mount_patch(v: &wire::MountPatch) -> Result<MountPatch, FromGrpcError> {
    Ok(MountPatch {
        provider: v.provider.as_ref().map(|x| provider_id(x)).transpose()?,
        auth: auth_patch(v.auth.as_ref())?,
        limits: limits_patch(v.limits.as_ref())?,
        config: bytes_patch(v.config.as_ref())?,
    })
}

fn to_mount_credential(v: &MountCredential) -> wire::MountCredential {
    wire::MountCredential {
        scheme: v.scheme.clone(),
        account_label: v.account_label.clone(),
    }
}

fn to_mount_limits(v: &MountLimits) -> wire::MountLimits {
    wire::MountLimits {
        max_memory_mb: v.max_memory_mb,
        max_fetch_blob_bytes: v.max_fetch_blob_bytes,
    }
}

fn to_auth_patch(v: &MountField<MountCredential>) -> wire::MountCredentialPatch {
    let value = match v {
        MountField::Keep => wire::mount_credential_patch::Value::Keep(wire::Empty {}),
        MountField::Set(x) => wire::mount_credential_patch::Value::Set(to_mount_credential(x)),
        MountField::Clear => wire::mount_credential_patch::Value::Clear(wire::Empty {}),
    };
    wire::MountCredentialPatch { value: Some(value) }
}

fn to_limits_patch(v: &MountField<MountLimits>) -> wire::MountLimitsPatch {
    let value = match v {
        MountField::Keep => wire::mount_limits_patch::Value::Keep(wire::Empty {}),
        MountField::Set(x) => wire::mount_limits_patch::Value::Set(to_mount_limits(x)),
        MountField::Clear => wire::mount_limits_patch::Value::Clear(wire::Empty {}),
    };
    wire::MountLimitsPatch { value: Some(value) }
}

fn to_bytes_patch(v: &MountField<Vec<u8>>) -> wire::BytesPatch {
    let value = match v {
        MountField::Keep => wire::bytes_patch::Value::Keep(wire::Empty {}),
        MountField::Set(x) => wire::bytes_patch::Value::Set(x.clone().into()),
        MountField::Clear => wire::bytes_patch::Value::Clear(wire::Empty {}),
    };
    wire::BytesPatch { value: Some(value) }
}

pub fn to_mount_patch(v: &MountPatch) -> wire::MountPatch {
    wire::MountPatch {
        provider: v.provider.map(|x| x.as_bytes().to_vec().into()),
        auth: Some(to_auth_patch(&v.auth)),
        limits: Some(to_limits_patch(&v.limits)),
        config: Some(to_bytes_patch(&v.config)),
    }
}

pub fn credential_key(v: &wire::CredentialKey) -> CredentialKey {
    CredentialKey {
        provider_name: v.provider_name.clone(),
        scheme: v.scheme.clone(),
        account_label: v.account_label.clone(),
    }
}
pub fn to_credential_key(v: &CredentialKey) -> wire::CredentialKey {
    wire::CredentialKey {
        provider_name: v.provider_name.clone(),
        scheme: v.scheme.clone(),
        account_label: v.account_label.clone(),
    }
}
fn credential_material(v: &wire::CredentialMaterial) -> Result<CredentialMaterial, FromGrpcError> {
    match req(v.value.clone(), "credential material")? {
        wire::credential_material::Value::StaticToken(x) => Ok(CredentialMaterial::StaticToken {
            token: SecretBytes::new(x.token.to_vec()),
        }),
        wire::credential_material::Value::Oauth(x) => Ok(CredentialMaterial::OAuth {
            access_token: SecretBytes::new(x.access_token.to_vec()),
            refresh_token: x.refresh_token.map(|x| SecretBytes::new(x.to_vec())),
            expires_at_unix: x.expires_at_unix,
            token_type: x.token_type,
            scopes: x.scopes,
            upstream_identity: x.upstream_identity,
        }),
    }
}
fn credential_overrides(v: &wire::CredentialClientOverrides) -> CredentialClientOverrides {
    CredentialClientOverrides {
        client_id: v.client_id.clone(),
        client_secret: v
            .client_secret
            .clone()
            .map(|x| SecretBytes::new(x.to_vec())),
        redirect_uri: v.redirect_uri.clone(),
        scopes: v.scopes.as_ref().map(|x| x.values.clone()),
    }
}
pub fn credential_submission(
    v: &wire::CredentialSubmission,
) -> Result<CredentialSubmission, FromGrpcError> {
    Ok(CredentialSubmission {
        provider: provider_id(&v.provider)?,
        scheme: v.scheme.clone(),
        account_label: v.account_label.clone(),
        material: credential_material(&req(v.material.clone(), "credential material")?)?,
        overrides: credential_overrides(&req(v.overrides.clone(), "credential overrides")?),
    })
}

pub fn to_credential_submission(v: &CredentialSubmission) -> wire::CredentialSubmission {
    wire::CredentialSubmission {
        provider: v.provider.as_bytes().to_vec().into(),
        scheme: v.scheme.clone(),
        account_label: v.account_label.clone(),
        material: Some(to_credential_material(&v.material)),
        overrides: Some(to_credential_overrides(&v.overrides)),
    }
}

fn to_credential_material(v: &CredentialMaterial) -> wire::CredentialMaterial {
    let value = match v {
        CredentialMaterial::StaticToken { token } => {
            wire::credential_material::Value::StaticToken(wire::StaticTokenMaterial {
                token: token.expose().to_vec().into(),
            })
        },
        CredentialMaterial::OAuth {
            access_token,
            refresh_token,
            expires_at_unix,
            token_type,
            scopes,
            upstream_identity,
        } => wire::credential_material::Value::Oauth(wire::OAuthMaterial {
            access_token: access_token.expose().to_vec().into(),
            refresh_token: refresh_token.as_ref().map(|x| x.expose().to_vec().into()),
            expires_at_unix: *expires_at_unix,
            token_type: token_type.clone(),
            scopes: scopes.clone(),
            upstream_identity: upstream_identity.clone(),
        }),
    };
    wire::CredentialMaterial { value: Some(value) }
}
fn to_credential_overrides(v: &CredentialClientOverrides) -> wire::CredentialClientOverrides {
    wire::CredentialClientOverrides {
        client_id: v.client_id.clone(),
        client_secret: v.client_secret.as_ref().map(|x| x.expose().to_vec().into()),
        redirect_uri: v.redirect_uri.clone(),
        scopes: v
            .scopes
            .as_ref()
            .map(|x| wire::StringList { values: x.clone() }),
    }
}

pub fn to_credential_status(v: &CredentialStatus) -> wire::CredentialStatus {
    wire::CredentialStatus {
        key: Some(to_credential_key(&v.key)),
        provider: v.provider.as_bytes().to_vec().into(),
        kind: match v.kind {
            CredentialKind::StaticToken => wire::CredentialKind::CredentialStaticToken as i32,
            CredentialKind::OAuth => wire::CredentialKind::CredentialOauth as i32,
        },
        scopes: v.scopes.clone(),
        auth_fingerprint: v.auth_fingerprint.as_bytes().to_vec().into(),
        version: v.version.get(),
        generation: v.generation.get(),
        status: match v.status {
            CredentialStatusKind::Active => wire::CredentialStatusKind::CredentialActive as i32,
            CredentialStatusKind::Blocked => wire::CredentialStatusKind::CredentialBlocked as i32,
            CredentialStatusKind::PendingRepublish => {
                wire::CredentialStatusKind::CredentialPendingRepublish as i32
            },
            CredentialStatusKind::RevocationPending => {
                wire::CredentialStatusKind::CredentialRevocationPending as i32
            },
            CredentialStatusKind::RevocationUnknown => {
                wire::CredentialStatusKind::CredentialRevocationUnknown as i32
            },
            CredentialStatusKind::Deleted => wire::CredentialStatusKind::CredentialDeleted as i32,
        },
        last_mutation_id: v.last_mutation_id.as_bytes().to_vec().into(),
    }
}
pub fn credential_status(v: &wire::CredentialStatus) -> Result<CredentialStatus, FromGrpcError> {
    let kind = match wire::CredentialKind::try_from(v.kind)
        .map_err(|_| FromGrpcError::Invalid("credential kind"))?
    {
        wire::CredentialKind::CredentialStaticToken => CredentialKind::StaticToken,
        wire::CredentialKind::CredentialOauth => CredentialKind::OAuth,
        wire::CredentialKind::Unspecified => {
            return Err(FromGrpcError::Unspecified("credential kind"));
        },
    };
    let status = match wire::CredentialStatusKind::try_from(v.status)
        .map_err(|_| FromGrpcError::Invalid("credential status"))?
    {
        wire::CredentialStatusKind::CredentialActive => CredentialStatusKind::Active,
        wire::CredentialStatusKind::CredentialBlocked => CredentialStatusKind::Blocked,
        wire::CredentialStatusKind::CredentialPendingRepublish => {
            CredentialStatusKind::PendingRepublish
        },
        wire::CredentialStatusKind::CredentialRevocationPending => {
            CredentialStatusKind::RevocationPending
        },
        wire::CredentialStatusKind::CredentialRevocationUnknown => {
            CredentialStatusKind::RevocationUnknown
        },
        wire::CredentialStatusKind::CredentialDeleted => CredentialStatusKind::Deleted,
        wire::CredentialStatusKind::Unspecified => {
            return Err(FromGrpcError::Unspecified("credential status"));
        },
    };
    Ok(CredentialStatus {
        key: credential_key(&req(v.key.clone(), "credential key")?),
        provider: provider_id(&v.provider)?,
        kind,
        scopes: v.scopes.clone(),
        auth_fingerprint: auth_fingerprint(&v.auth_fingerprint)?,
        version: CredentialVersion::new(nz(v.version, "credential version")?),
        generation: CredentialGeneration::new(nz(v.generation, "credential generation")?),
        status,
        last_mutation_id: mutation_id(&v.last_mutation_id)?,
    })
}

fn to_filesystem_spec(v: &omnifs_core::fs::Spec) -> wire::FilesystemSpec {
    wire::FilesystemSpec {
        id: v.id().to_string(),
        protocol: match v.protocol() {
            omnifs_core::fs::Protocol::Fuse => wire::FsProtocol::FsFuse as i32,
            omnifs_core::fs::Protocol::Nfs => wire::FsProtocol::FsNfs as i32,
        },
        runtime: match v.runtime() {
            omnifs_core::fs::Runtime::Host => wire::FsRuntime::FsHost as i32,
            omnifs_core::fs::Runtime::Docker => wire::FsRuntime::FsDocker as i32,
            omnifs_core::fs::Runtime::Libkrun => wire::FsRuntime::FsLibkrun as i32,
        },
        location: path_bytes(v.location()).into(),
    }
}
fn filesystem_spec(v: &wire::FilesystemSpec) -> Result<omnifs_core::fs::Spec, FromGrpcError> {
    let id = omnifs_core::fs::Id::new(v.id.clone())
        .map_err(|_| FromGrpcError::Invalid("filesystem id"))?;
    let protocol = match wire::FsProtocol::try_from(v.protocol)
        .map_err(|_| FromGrpcError::Invalid("filesystem protocol"))?
    {
        wire::FsProtocol::FsFuse => omnifs_core::fs::Protocol::Fuse,
        wire::FsProtocol::FsNfs => omnifs_core::fs::Protocol::Nfs,
        wire::FsProtocol::Unspecified => {
            return Err(FromGrpcError::Unspecified("filesystem protocol"));
        },
    };
    let runtime = match wire::FsRuntime::try_from(v.runtime)
        .map_err(|_| FromGrpcError::Invalid("filesystem runtime"))?
    {
        wire::FsRuntime::FsHost => omnifs_core::fs::Runtime::Host,
        wire::FsRuntime::FsDocker => omnifs_core::fs::Runtime::Docker,
        wire::FsRuntime::FsLibkrun => omnifs_core::fs::Runtime::Libkrun,
        wire::FsRuntime::Unspecified => {
            return Err(FromGrpcError::Unspecified("filesystem runtime"));
        },
    };
    omnifs_core::fs::Spec::new(
        id,
        protocol,
        runtime,
        path_from_bytes(&v.location, "filesystem location")?,
    )
    .map_err(|_| FromGrpcError::Path("filesystem location"))
}

fn action_id(v: &[u8]) -> Result<ActionId, FromGrpcError> {
    Ok(ActionId::from_bytes(exact(v, "action id")?))
}

fn resource_digest(v: &[u8]) -> Result<ResourceDigest, FromGrpcError> {
    Ok(ResourceDigest::from_bytes(exact(v, "resource digest")?))
}

fn resource_name(v: String) -> Result<ResourceName, FromGrpcError> {
    ResourceName::new(v).map_err(|_| FromGrpcError::Invalid("resource name"))
}

fn resource_kind(v: wire::ResourceKind) -> Result<ResourceKind, FromGrpcError> {
    match v {
        wire::ResourceKind::ResourceProvider => Ok(ResourceKind::Provider),
        wire::ResourceKind::ResourceCredential => Ok(ResourceKind::Credential),
        wire::ResourceKind::ResourceMount => Ok(ResourceKind::Mount),
        wire::ResourceKind::ResourceAttachment => Ok(ResourceKind::Attachment),
        wire::ResourceKind::Unspecified => Err(FromGrpcError::Unspecified("resource kind")),
    }
}

fn to_resource_kind(v: ResourceKind) -> i32 {
    match v {
        ResourceKind::Provider => wire::ResourceKind::ResourceProvider as i32,
        ResourceKind::Credential => wire::ResourceKind::ResourceCredential as i32,
        ResourceKind::Mount => wire::ResourceKind::ResourceMount as i32,
        ResourceKind::Attachment => wire::ResourceKind::ResourceAttachment as i32,
    }
}

fn resource_key(v: &wire::ResourceKey) -> Result<ResourceKey, FromGrpcError> {
    Ok(ResourceKey::new(
        resource_kind(
            wire::ResourceKind::try_from(v.kind)
                .map_err(|_| FromGrpcError::Invalid("resource kind"))?,
        )?,
        resource_name(v.name.clone())?,
    ))
}

fn to_resource_key(v: &ResourceKey) -> wire::ResourceKey {
    wire::ResourceKey {
        kind: to_resource_kind(v.kind),
        name: v.name.to_string(),
    }
}

fn resource_phase(v: wire::ResourcePhase) -> Result<ResourcePhase, FromGrpcError> {
    match v {
        wire::ResourcePhase::ResourcePending => Ok(ResourcePhase::Pending),
        wire::ResourcePhase::ResourcePreparing => Ok(ResourcePhase::Preparing),
        wire::ResourcePhase::ResourceReady => Ok(ResourcePhase::Ready),
        wire::ResourcePhase::ResourceRetrying => Ok(ResourcePhase::Retrying),
        wire::ResourcePhase::ResourceFailed => Ok(ResourcePhase::Failed),
        wire::ResourcePhase::ResourceBlocked => Ok(ResourcePhase::Blocked),
        wire::ResourcePhase::ResourceDeleting => Ok(ResourcePhase::Deleting),
        wire::ResourcePhase::Unspecified => Err(FromGrpcError::Unspecified("resource phase")),
    }
}

fn to_resource_phase(v: ResourcePhase) -> i32 {
    match v {
        ResourcePhase::Pending => wire::ResourcePhase::ResourcePending as i32,
        ResourcePhase::Preparing => wire::ResourcePhase::ResourcePreparing as i32,
        ResourcePhase::Ready => wire::ResourcePhase::ResourceReady as i32,
        ResourcePhase::Retrying => wire::ResourcePhase::ResourceRetrying as i32,
        ResourcePhase::Failed => wire::ResourcePhase::ResourceFailed as i32,
        ResourcePhase::Blocked => wire::ResourcePhase::ResourceBlocked as i32,
        ResourcePhase::Deleting => wire::ResourcePhase::ResourceDeleting as i32,
    }
}

fn resource_change_action(
    v: wire::ResourceChangeAction,
) -> Result<ResourceChangeAction, FromGrpcError> {
    match v {
        wire::ResourceChangeAction::ResourceCreate => Ok(ResourceChangeAction::Create),
        wire::ResourceChangeAction::ResourceUpdate => Ok(ResourceChangeAction::Update),
        wire::ResourceChangeAction::ResourceDelete => Ok(ResourceChangeAction::Delete),
        wire::ResourceChangeAction::ResourceUnchanged => Ok(ResourceChangeAction::Unchanged),
        wire::ResourceChangeAction::Unspecified => {
            Err(FromGrpcError::Unspecified("resource change action"))
        },
    }
}

fn to_resource_change_action(v: ResourceChangeAction) -> i32 {
    match v {
        ResourceChangeAction::Create => wire::ResourceChangeAction::ResourceCreate as i32,
        ResourceChangeAction::Update => wire::ResourceChangeAction::ResourceUpdate as i32,
        ResourceChangeAction::Delete => wire::ResourceChangeAction::ResourceDelete as i32,
        ResourceChangeAction::Unchanged => wire::ResourceChangeAction::ResourceUnchanged as i32,
    }
}

fn attachment_spec(v: &wire::AttachmentSpec) -> Result<AttachmentSpec, FromGrpcError> {
    let protocol = match wire::FsProtocol::try_from(v.protocol)
        .map_err(|_| FromGrpcError::Invalid("attachment protocol"))?
    {
        wire::FsProtocol::FsFuse => omnifs_core::fs::Protocol::Fuse,
        wire::FsProtocol::FsNfs => omnifs_core::fs::Protocol::Nfs,
        wire::FsProtocol::Unspecified => {
            return Err(FromGrpcError::Unspecified("attachment protocol"));
        },
    };
    let runtime = match wire::FsRuntime::try_from(v.runtime)
        .map_err(|_| FromGrpcError::Invalid("attachment runtime"))?
    {
        wire::FsRuntime::FsHost => omnifs_core::fs::Runtime::Host,
        wire::FsRuntime::FsDocker => omnifs_core::fs::Runtime::Docker,
        wire::FsRuntime::FsLibkrun => omnifs_core::fs::Runtime::Libkrun,
        wire::FsRuntime::Unspecified => {
            return Err(FromGrpcError::Unspecified("attachment runtime"));
        },
    };
    AttachmentSpec::new(
        protocol,
        runtime,
        path_from_bytes(&v.location, "attachment location")?,
        v.docker_image.clone(),
        v.libkrun_guest_image.clone(),
    )
    .map_err(|_| FromGrpcError::Invalid("attachment spec"))
}

fn to_attachment_spec(v: &AttachmentSpec) -> wire::AttachmentSpec {
    wire::AttachmentSpec {
        protocol: match v.protocol() {
            omnifs_core::fs::Protocol::Fuse => wire::FsProtocol::FsFuse as i32,
            omnifs_core::fs::Protocol::Nfs => wire::FsProtocol::FsNfs as i32,
        },
        runtime: match v.runtime() {
            omnifs_core::fs::Runtime::Host => wire::FsRuntime::FsHost as i32,
            omnifs_core::fs::Runtime::Docker => wire::FsRuntime::FsDocker as i32,
            omnifs_core::fs::Runtime::Libkrun => wire::FsRuntime::FsLibkrun as i32,
        },
        location: path_bytes(v.location()).into(),
        docker_image: v.docker_image().map(str::to_owned),
        libkrun_guest_image: v.libkrun_guest_image().map(str::to_owned),
    }
}

fn resource_limits(v: &wire::ResourceLimits) -> ResourceLimits {
    ResourceLimits {
        max_memory_mb: v.max_memory_mb,
        max_fetch_blob_bytes: v.max_fetch_blob_bytes,
    }
}

fn to_resource_limits(v: &ResourceLimits) -> wire::ResourceLimits {
    wire::ResourceLimits {
        max_memory_mb: v.max_memory_mb,
        max_fetch_blob_bytes: v.max_fetch_blob_bytes,
    }
}

fn resource_definition(v: &wire::ResourceDefinition) -> Result<ResourceDefinition, FromGrpcError> {
    match req(v.definition.clone(), "resource definition")? {
        wire::resource_definition::Definition::Provider(value) => {
            Ok(ResourceDefinition::Provider(ProviderDefinition {
                name: resource_name(value.name)?,
                artifact: provider_id(&value.artifact)?,
            }))
        },
        wire::resource_definition::Definition::Credential(value) => {
            Ok(ResourceDefinition::Credential(CredentialDefinition {
                name: resource_name(value.name)?,
                provider: resource_name(value.provider)?,
                scheme: value.scheme,
                account: value.account,
            }))
        },
        wire::resource_definition::Definition::Mount(value) => {
            let config = serde_json::from_slice(&value.config)
                .map_err(|error| FromGrpcError::Json(error.to_string()))?;
            Ok(ResourceDefinition::Mount(MountResourceDefinition {
                name: resource_name(value.name)?,
                provider: resource_name(value.provider)?,
                credential: value.credential.map(resource_name).transpose()?,
                config,
                limits: value.limits.as_ref().map(resource_limits),
            }))
        },
        wire::resource_definition::Definition::Attachment(value) => {
            Ok(ResourceDefinition::Attachment(AttachmentDefinition {
                name: resource_name(value.name)?,
                spec: attachment_spec(&req(value.spec, "attachment spec")?)?,
            }))
        },
    }
}

fn to_resource_definition(v: &ResourceDefinition) -> wire::ResourceDefinition {
    let definition = match v {
        ResourceDefinition::Provider(value) => {
            wire::resource_definition::Definition::Provider(wire::ProviderResource {
                name: value.name.to_string(),
                artifact: value.artifact.as_bytes().to_vec().into(),
            })
        },
        ResourceDefinition::Credential(value) => {
            wire::resource_definition::Definition::Credential(wire::CredentialResource {
                name: value.name.to_string(),
                provider: value.provider.to_string(),
                scheme: value.scheme.clone(),
                account: value.account.clone(),
            })
        },
        ResourceDefinition::Mount(value) => {
            wire::resource_definition::Definition::Mount(wire::MountResource {
                name: value.name.to_string(),
                provider: value.provider.to_string(),
                credential: value.credential.as_ref().map(ToString::to_string),
                config: serde_json::to_vec(&value.config)
                    .expect("validated resource configuration serializes")
                    .into(),
                limits: value.limits.as_ref().map(to_resource_limits),
            })
        },
        ResourceDefinition::Attachment(value) => {
            wire::resource_definition::Definition::Attachment(wire::AttachmentResource {
                name: value.name.to_string(),
                spec: Some(to_attachment_spec(&value.spec)),
            })
        },
    };
    wire::ResourceDefinition {
        definition: Some(definition),
    }
}

fn resource_count(count: usize) -> Result<(), FromGrpcError> {
    if count > CONTROL_RESOURCE_MAX_COUNT {
        return Err(FromGrpcError::TooManyResources {
            count,
            max: CONTROL_RESOURCE_MAX_COUNT,
        });
    }
    Ok(())
}

pub fn resource_declarations(
    v: &wire::ResourceDeclarations,
) -> Result<ResourceDeclarations, FromGrpcError> {
    resource_count(v.resources.len())?;
    if v.api_version != crate::API_VERSION {
        return Err(FromGrpcError::UnsupportedApiVersion(v.api_version.clone()));
    }
    let declarations = ResourceDeclarations {
        api_version: v.api_version.clone(),
        resources: v
            .resources
            .iter()
            .map(resource_definition)
            .collect::<Result<_, _>>()?,
    };
    let normalized = declarations
        .normalize()
        .map_err(|_| FromGrpcError::Invalid("resource declarations"))?;
    Ok(ResourceDeclarations {
        api_version: crate::API_VERSION.into(),
        resources: normalized.resources().to_vec(),
    })
}

pub fn to_resource_declarations(v: &ResourceDeclarations) -> wire::ResourceDeclarations {
    wire::ResourceDeclarations {
        api_version: v.api_version.clone(),
        resources: v.resources.iter().map(to_resource_definition).collect(),
    }
}

pub fn get_resources_response(
    v: &wire::GetResourcesResponse,
) -> Result<ResourceSnapshot, FromGrpcError> {
    resource_snapshot(&req(v.snapshot.clone(), "resource snapshot")?)
}

pub fn to_get_resources_response(v: &ResourceSnapshot) -> wire::GetResourcesResponse {
    wire::GetResourcesResponse {
        snapshot: Some(to_resource_snapshot(v)),
    }
}

pub fn plan_resources_request(
    v: &wire::PlanResourcesRequest,
) -> Result<ResourceDeclarations, FromGrpcError> {
    resource_declarations(&req(v.declarations.clone(), "resource declarations")?)
}

pub fn to_plan_resources_request(v: &ResourceDeclarations) -> wire::PlanResourcesRequest {
    wire::PlanResourcesRequest {
        declarations: Some(to_resource_declarations(v)),
    }
}

pub fn plan_resources_response(
    v: &wire::PlanResourcesResponse,
) -> Result<ResourcePlan, FromGrpcError> {
    resource_plan(&req(v.plan.clone(), "resource plan")?)
}

pub fn to_plan_resources_response(v: &ResourcePlan) -> wire::PlanResourcesResponse {
    wire::PlanResourcesResponse {
        plan: Some(to_resource_plan(v)),
    }
}

fn resource_status(v: &wire::ResourceStatus) -> Result<ResourceStatus, FromGrpcError> {
    Ok(ResourceStatus {
        key: resource_key(&req(v.key.clone(), "resource status key")?)?,
        desired_revision: ResourceRevision::new(v.desired_revision),
        observed_revision: v.observed_revision.map(ResourceRevision::new),
        phase: resource_phase(
            wire::ResourcePhase::try_from(v.phase)
                .map_err(|_| FromGrpcError::Invalid("resource phase"))?,
        )?,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
    })
}

fn to_resource_status(v: &ResourceStatus) -> wire::ResourceStatus {
    wire::ResourceStatus {
        key: Some(to_resource_key(&v.key)),
        desired_revision: v.desired_revision.get(),
        observed_revision: v.observed_revision.map(ResourceRevision::get),
        phase: to_resource_phase(v.phase),
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
    }
}

fn resource_change(v: &wire::ResourceChange) -> Result<ResourceChange, FromGrpcError> {
    Ok(ResourceChange {
        key: resource_key(&req(v.key.clone(), "resource change key")?)?,
        action: resource_change_action(
            wire::ResourceChangeAction::try_from(v.action)
                .map_err(|_| FromGrpcError::Invalid("resource change action"))?,
        )?,
        destructive: v.destructive,
        secret_impact: v.secret_impact,
    })
}

fn to_resource_change(v: &ResourceChange) -> wire::ResourceChange {
    wire::ResourceChange {
        key: Some(to_resource_key(&v.key)),
        action: to_resource_change_action(v.action),
        destructive: v.destructive,
        secret_impact: v.secret_impact,
    }
}

pub fn resource_plan(v: &wire::ResourcePlan) -> Result<ResourcePlan, FromGrpcError> {
    resource_count(v.normalized.len())?;
    resource_count(v.changes.len())?;
    let declarations = ResourceDeclarations {
        api_version: crate::API_VERSION.into(),
        resources: v
            .normalized
            .iter()
            .map(resource_definition)
            .collect::<Result<_, _>>()?,
    };
    let normalized = declarations
        .normalize()
        .map_err(|_| FromGrpcError::Invalid("resource plan declarations"))?;
    let desired_digest = resource_digest(&v.desired_digest)?;
    if normalized.digest() != desired_digest {
        return Err(FromGrpcError::Invalid("resource plan digest"));
    }
    Ok(ResourcePlan {
        base_revision: ResourceRevision::new(v.base_revision),
        desired_digest,
        normalized: normalized.resources().to_vec(),
        changes: v
            .changes
            .iter()
            .map(resource_change)
            .collect::<Result<_, _>>()?,
    })
}

pub fn to_resource_plan(v: &ResourcePlan) -> wire::ResourcePlan {
    wire::ResourcePlan {
        base_revision: v.base_revision.get(),
        desired_digest: v.desired_digest.as_bytes().to_vec().into(),
        normalized: v.normalized.iter().map(to_resource_definition).collect(),
        changes: v.changes.iter().map(to_resource_change).collect(),
    }
}

pub fn apply_receipt(v: &wire::ApplyReceipt) -> Result<ApplyReceipt, FromGrpcError> {
    Ok(ApplyReceipt {
        mutation_id: mutation_id(&v.mutation_id)?,
        revision: ResourceRevision::new(v.revision),
        desired_digest: resource_digest(&v.desired_digest)?,
        created: v.created,
        updated: v.updated,
        deleted: v.deleted,
        changed: v.changed,
    })
}

pub fn to_apply_receipt(v: &ApplyReceipt) -> wire::ApplyReceipt {
    wire::ApplyReceipt {
        mutation_id: v.mutation_id.as_bytes().to_vec().into(),
        revision: v.revision.get(),
        desired_digest: v.desired_digest.as_bytes().to_vec().into(),
        created: v.created,
        updated: v.updated,
        deleted: v.deleted,
        changed: v.changed,
    }
}

fn credential_material_sidecar(
    v: &wire::CredentialMaterialSidecar,
) -> Result<CredentialMaterialSidecar, FromGrpcError> {
    Ok(CredentialMaterialSidecar {
        credential: resource_name(v.credential_name.clone())?,
        material: credential_material(&req(v.material.clone(), "credential material")?)?,
        overrides: credential_overrides(&req(v.overrides.clone(), "credential overrides")?),
    })
}

fn to_credential_material_sidecar(
    v: &CredentialMaterialSidecar,
) -> wire::CredentialMaterialSidecar {
    wire::CredentialMaterialSidecar {
        credential_name: v.credential.to_string(),
        material: Some(to_credential_material(&v.material)),
        overrides: Some(to_credential_overrides(&v.overrides)),
    }
}

pub fn apply_resources_request(
    v: &wire::ApplyResourcesRequest,
) -> Result<ApplyResourcesRequest, FromGrpcError> {
    resource_count(v.credential_material.len())?;
    Ok(ApplyResourcesRequest {
        mutation_id: mutation_id(&v.mutation_id)?,
        base_revision: ResourceRevision::new(v.base_revision),
        expected_desired_digest: resource_digest(&v.expected_desired_digest)?,
        declarations: resource_declarations(&req(
            v.declarations.clone(),
            "resource declarations",
        )?)?,
        credential_material: v
            .credential_material
            .iter()
            .map(credential_material_sidecar)
            .collect::<Result<_, _>>()?,
    })
}

pub fn to_apply_resources_request(v: &ApplyResourcesRequest) -> wire::ApplyResourcesRequest {
    wire::ApplyResourcesRequest {
        mutation_id: v.mutation_id.as_bytes().to_vec().into(),
        base_revision: v.base_revision.get(),
        expected_desired_digest: v.expected_desired_digest.as_bytes().to_vec().into(),
        declarations: Some(to_resource_declarations(&v.declarations)),
        credential_material: v
            .credential_material
            .iter()
            .map(to_credential_material_sidecar)
            .collect(),
    }
}

pub fn apply_resources_response(
    v: &wire::ApplyResourcesResponse,
) -> Result<ApplyReceipt, FromGrpcError> {
    apply_receipt(&req(v.receipt.clone(), "apply receipt")?)
}

pub fn to_apply_resources_response(v: &ApplyReceipt) -> wire::ApplyResourcesResponse {
    wire::ApplyResourcesResponse {
        receipt: Some(to_apply_receipt(v)),
    }
}

pub fn resource_snapshot(v: &wire::ResourceSnapshot) -> Result<ResourceSnapshot, FromGrpcError> {
    resource_count(v.resources.len())?;
    resource_count(v.resources_status.len())?;
    let declarations = ResourceDeclarations {
        api_version: crate::API_VERSION.into(),
        resources: v
            .resources
            .iter()
            .map(resource_definition)
            .collect::<Result<_, _>>()?,
    };
    let resources = declarations
        .normalize()
        .map_err(|_| FromGrpcError::Invalid("resource snapshot declarations"))?
        .resources()
        .to_vec();
    let desired_digest = resource_digest(&v.desired_digest)?;
    if NormalizedResourceSet::new(resources.clone())
        .map_err(|_| FromGrpcError::Invalid("resource snapshot declarations"))?
        .digest()
        != desired_digest
    {
        return Err(FromGrpcError::Invalid("resource snapshot digest"));
    }
    Ok(ResourceSnapshot {
        revision: ResourceRevision::new(v.revision),
        desired_digest,
        resources,
        resource_statuses: v
            .resources_status
            .iter()
            .map(resource_status)
            .collect::<Result<_, _>>()?,
    })
}

pub fn to_resource_snapshot(v: &ResourceSnapshot) -> wire::ResourceSnapshot {
    wire::ResourceSnapshot {
        revision: v.revision.get(),
        desired_digest: v.desired_digest.as_bytes().to_vec().into(),
        resources: v.resources.iter().map(to_resource_definition).collect(),
        resources_status: v.resource_statuses.iter().map(to_resource_status).collect(),
    }
}

fn action_kind(v: wire::ActionKind) -> Result<ActionKind, FromGrpcError> {
    match v {
        wire::ActionKind::ActionSetCredentialMaterial => Ok(ActionKind::SetCredentialMaterial),
        wire::ActionKind::ActionRevokeCredential => Ok(ActionKind::RevokeCredential),
        wire::ActionKind::ActionRestartAttachment => Ok(ActionKind::RestartAttachment),
        wire::ActionKind::Unspecified => Err(FromGrpcError::Unspecified("action kind")),
    }
}

fn to_action_kind(v: ActionKind) -> i32 {
    match v {
        ActionKind::SetCredentialMaterial => wire::ActionKind::ActionSetCredentialMaterial as i32,
        ActionKind::RevokeCredential => wire::ActionKind::ActionRevokeCredential as i32,
        ActionKind::RestartAttachment => wire::ActionKind::ActionRestartAttachment as i32,
    }
}

fn action_phase(v: wire::ActionPhase) -> Result<ActionPhase, FromGrpcError> {
    match v {
        wire::ActionPhase::ActionAccepted => Ok(ActionPhase::Accepted),
        wire::ActionPhase::ActionRunning => Ok(ActionPhase::Running),
        wire::ActionPhase::ActionRetrying => Ok(ActionPhase::Retrying),
        wire::ActionPhase::ActionReady => Ok(ActionPhase::Ready),
        wire::ActionPhase::ActionFailed => Ok(ActionPhase::Failed),
        wire::ActionPhase::Unspecified => Err(FromGrpcError::Unspecified("action phase")),
    }
}

fn to_action_phase(v: ActionPhase) -> i32 {
    match v {
        ActionPhase::Accepted => wire::ActionPhase::ActionAccepted as i32,
        ActionPhase::Running => wire::ActionPhase::ActionRunning as i32,
        ActionPhase::Retrying => wire::ActionPhase::ActionRetrying as i32,
        ActionPhase::Ready => wire::ActionPhase::ActionReady as i32,
        ActionPhase::Failed => wire::ActionPhase::ActionFailed as i32,
    }
}

fn validate_action_target(kind: ActionKind, target: &ResourceKey) -> Result<(), FromGrpcError> {
    let expected = match kind {
        ActionKind::SetCredentialMaterial | ActionKind::RevokeCredential => {
            ResourceKind::Credential
        },
        ActionKind::RestartAttachment => ResourceKind::Attachment,
    };
    if target.kind != expected {
        return Err(FromGrpcError::Invalid("action target"));
    }
    Ok(())
}

pub fn action_receipt(v: &wire::ActionReceipt) -> Result<ActionReceipt, FromGrpcError> {
    if v.action_generation == 0 {
        return Err(FromGrpcError::Zero("action generation"));
    }
    let kind = action_kind(
        wire::ActionKind::try_from(v.kind).map_err(|_| FromGrpcError::Invalid("action kind"))?,
    )?;
    let target = resource_key(&req(v.target.clone(), "action target")?)?;
    validate_action_target(kind, &target)?;
    Ok(ActionReceipt {
        action_id: action_id(&v.action_id)?,
        kind,
        target,
        action_generation: v.action_generation,
        phase: action_phase(
            wire::ActionPhase::try_from(v.phase)
                .map_err(|_| FromGrpcError::Invalid("action phase"))?,
        )?,
    })
}

pub fn to_action_receipt(v: &ActionReceipt) -> wire::ActionReceipt {
    wire::ActionReceipt {
        action_id: v.action_id.as_bytes().to_vec().into(),
        kind: to_action_kind(v.kind),
        target: Some(to_resource_key(&v.target)),
        action_generation: v.action_generation,
        phase: to_action_phase(v.phase),
    }
}

fn credential_status_kind(
    v: wire::CredentialStatusKind,
) -> Result<CredentialStatusKind, FromGrpcError> {
    match v {
        wire::CredentialStatusKind::CredentialActive => Ok(CredentialStatusKind::Active),
        wire::CredentialStatusKind::CredentialBlocked => Ok(CredentialStatusKind::Blocked),
        wire::CredentialStatusKind::CredentialPendingRepublish => {
            Ok(CredentialStatusKind::PendingRepublish)
        },
        wire::CredentialStatusKind::CredentialRevocationPending => {
            Ok(CredentialStatusKind::RevocationPending)
        },
        wire::CredentialStatusKind::CredentialRevocationUnknown => {
            Ok(CredentialStatusKind::RevocationUnknown)
        },
        wire::CredentialStatusKind::CredentialDeleted => Ok(CredentialStatusKind::Deleted),
        wire::CredentialStatusKind::Unspecified => {
            Err(FromGrpcError::Unspecified("credential status"))
        },
    }
}

fn to_credential_status_kind(v: CredentialStatusKind) -> i32 {
    match v {
        CredentialStatusKind::Active => wire::CredentialStatusKind::CredentialActive as i32,
        CredentialStatusKind::Blocked => wire::CredentialStatusKind::CredentialBlocked as i32,
        CredentialStatusKind::PendingRepublish => {
            wire::CredentialStatusKind::CredentialPendingRepublish as i32
        },
        CredentialStatusKind::RevocationPending => {
            wire::CredentialStatusKind::CredentialRevocationPending as i32
        },
        CredentialStatusKind::RevocationUnknown => {
            wire::CredentialStatusKind::CredentialRevocationUnknown as i32
        },
        CredentialStatusKind::Deleted => wire::CredentialStatusKind::CredentialDeleted as i32,
    }
}

pub fn credential_receipt(v: &wire::CredentialReceipt) -> Result<CredentialReceipt, FromGrpcError> {
    let action = action_receipt(&req(v.action.clone(), "action receipt")?)?;
    if !matches!(
        action.kind,
        ActionKind::SetCredentialMaterial | ActionKind::RevokeCredential
    ) {
        return Err(FromGrpcError::Invalid("credential action kind"));
    }
    Ok(CredentialReceipt {
        action,
        status: credential_status_kind(
            wire::CredentialStatusKind::try_from(v.status)
                .map_err(|_| FromGrpcError::Invalid("credential status"))?,
        )?,
    })
}

pub fn to_credential_receipt(v: &CredentialReceipt) -> wire::CredentialReceipt {
    wire::CredentialReceipt {
        action: Some(to_action_receipt(&v.action)),
        status: to_credential_status_kind(v.status),
    }
}

pub fn set_credential_material_response(
    v: &wire::SetCredentialMaterialResponse,
) -> Result<CredentialReceipt, FromGrpcError> {
    credential_receipt(&req(v.receipt.clone(), "credential receipt")?)
}

pub fn to_set_credential_material_response(
    v: &CredentialReceipt,
) -> wire::SetCredentialMaterialResponse {
    wire::SetCredentialMaterialResponse {
        receipt: Some(to_credential_receipt(v)),
    }
}

pub fn revoke_credential_response(
    v: &wire::RevokeCredentialResponse,
) -> Result<CredentialReceipt, FromGrpcError> {
    credential_receipt(&req(v.receipt.clone(), "credential receipt")?)
}

pub fn to_revoke_credential_response(v: &CredentialReceipt) -> wire::RevokeCredentialResponse {
    wire::RevokeCredentialResponse {
        receipt: Some(to_credential_receipt(v)),
    }
}

pub fn set_credential_material_request(
    v: &wire::SetCredentialMaterialRequest,
) -> Result<SetCredentialMaterialRequest, FromGrpcError> {
    Ok(SetCredentialMaterialRequest {
        action_id: action_id(&v.action_id)?,
        base_action_generation: v.base_action_generation,
        credential: resource_name(v.credential_name.clone())?,
        material: credential_material(&req(v.material.clone(), "credential material")?)?,
        overrides: credential_overrides(&req(v.overrides.clone(), "credential overrides")?),
    })
}

pub fn to_set_credential_material_request(
    v: &SetCredentialMaterialRequest,
) -> wire::SetCredentialMaterialRequest {
    wire::SetCredentialMaterialRequest {
        action_id: v.action_id.as_bytes().to_vec().into(),
        base_action_generation: v.base_action_generation,
        credential_name: v.credential.to_string(),
        material: Some(to_credential_material(&v.material)),
        overrides: Some(to_credential_overrides(&v.overrides)),
    }
}

pub fn revoke_credential_request(
    v: &wire::RevokeCredentialRequest,
) -> Result<RevokeCredentialRequest, FromGrpcError> {
    Ok(RevokeCredentialRequest {
        action_id: action_id(&v.action_id)?,
        base_action_generation: v.base_action_generation,
        credential: resource_name(v.credential_name.clone())?,
    })
}

pub fn to_revoke_credential_request(v: &RevokeCredentialRequest) -> wire::RevokeCredentialRequest {
    wire::RevokeCredentialRequest {
        action_id: v.action_id.as_bytes().to_vec().into(),
        base_action_generation: v.base_action_generation,
        credential_name: v.credential.to_string(),
    }
}

pub fn progress_target(v: &wire::WatchProgressRequest) -> Result<ProgressTarget, FromGrpcError> {
    match req(v.target.clone(), "progress target")? {
        wire::watch_progress_request::Target::DesiredRevision(revision) => Ok(
            ProgressTarget::DesiredRevision(ResourceRevision::new(revision)),
        ),
        wire::watch_progress_request::Target::ActionId(id) => {
            Ok(ProgressTarget::Action(action_id(&id)?))
        },
        wire::watch_progress_request::Target::Current(_) => Ok(ProgressTarget::Current),
    }
}

pub fn to_progress_target(v: ProgressTarget) -> wire::WatchProgressRequest {
    let target = match v {
        ProgressTarget::DesiredRevision(revision) => {
            wire::watch_progress_request::Target::DesiredRevision(revision.get())
        },
        ProgressTarget::Action(id) => {
            wire::watch_progress_request::Target::ActionId(id.as_bytes().to_vec().into())
        },
        ProgressTarget::Current => wire::watch_progress_request::Target::Current(wire::Empty {}),
    };
    wire::WatchProgressRequest {
        target: Some(target),
    }
}

fn provider_preparation_stage(
    v: wire::ProviderPreparationStage,
) -> Result<ProviderPreparationStage, FromGrpcError> {
    match v {
        wire::ProviderPreparationStage::ProviderPreparationQueued => {
            Ok(ProviderPreparationStage::Queued)
        },
        wire::ProviderPreparationStage::ProviderPreparationFetching => {
            Ok(ProviderPreparationStage::Fetching)
        },
        wire::ProviderPreparationStage::ProviderPreparationValidating => {
            Ok(ProviderPreparationStage::Validating)
        },
        wire::ProviderPreparationStage::ProviderPreparationCompiling => {
            Ok(ProviderPreparationStage::Compiling)
        },
        wire::ProviderPreparationStage::ProviderPreparationReady => {
            Ok(ProviderPreparationStage::Ready)
        },
        wire::ProviderPreparationStage::ProviderPreparationFailed => {
            Ok(ProviderPreparationStage::Failed)
        },
        wire::ProviderPreparationStage::Unspecified => {
            Err(FromGrpcError::Unspecified("provider preparation stage"))
        },
    }
}

fn to_provider_preparation_stage(v: ProviderPreparationStage) -> i32 {
    match v {
        ProviderPreparationStage::Queued => {
            wire::ProviderPreparationStage::ProviderPreparationQueued as i32
        },
        ProviderPreparationStage::Fetching => {
            wire::ProviderPreparationStage::ProviderPreparationFetching as i32
        },
        ProviderPreparationStage::Validating => {
            wire::ProviderPreparationStage::ProviderPreparationValidating as i32
        },
        ProviderPreparationStage::Compiling => {
            wire::ProviderPreparationStage::ProviderPreparationCompiling as i32
        },
        ProviderPreparationStage::Ready => {
            wire::ProviderPreparationStage::ProviderPreparationReady as i32
        },
        ProviderPreparationStage::Failed => {
            wire::ProviderPreparationStage::ProviderPreparationFailed as i32
        },
    }
}

fn serving_progress_stage(
    v: wire::ServingProgressStage,
) -> Result<ServingProgressStage, FromGrpcError> {
    match v {
        wire::ServingProgressStage::ServingQueued => Ok(ServingProgressStage::Queued),
        wire::ServingProgressStage::ServingBuilding => Ok(ServingProgressStage::Building),
        wire::ServingProgressStage::ServingDraining => Ok(ServingProgressStage::Draining),
        wire::ServingProgressStage::ServingReady => Ok(ServingProgressStage::Ready),
        wire::ServingProgressStage::ServingFailed => Ok(ServingProgressStage::Failed),
        wire::ServingProgressStage::Unspecified => {
            Err(FromGrpcError::Unspecified("serving progress stage"))
        },
    }
}

fn to_serving_progress_stage(v: ServingProgressStage) -> i32 {
    match v {
        ServingProgressStage::Queued => wire::ServingProgressStage::ServingQueued as i32,
        ServingProgressStage::Building => wire::ServingProgressStage::ServingBuilding as i32,
        ServingProgressStage::Draining => wire::ServingProgressStage::ServingDraining as i32,
        ServingProgressStage::Ready => wire::ServingProgressStage::ServingReady as i32,
        ServingProgressStage::Failed => wire::ServingProgressStage::ServingFailed as i32,
    }
}

fn credential_progress_stage(
    v: wire::CredentialProgressStage,
) -> Result<CredentialProgressStage, FromGrpcError> {
    match v {
        wire::CredentialProgressStage::CredentialProgressQueued => {
            Ok(CredentialProgressStage::Queued)
        },
        wire::CredentialProgressStage::CredentialProgressRefreshing => {
            Ok(CredentialProgressStage::Refreshing)
        },
        wire::CredentialProgressStage::CredentialProgressRevoking => {
            Ok(CredentialProgressStage::Revoking)
        },
        wire::CredentialProgressStage::CredentialProgressReady => {
            Ok(CredentialProgressStage::Ready)
        },
        wire::CredentialProgressStage::CredentialProgressFailed => {
            Ok(CredentialProgressStage::Failed)
        },
        wire::CredentialProgressStage::Unspecified => {
            Err(FromGrpcError::Unspecified("credential progress stage"))
        },
    }
}

fn to_credential_progress_stage(v: CredentialProgressStage) -> i32 {
    match v {
        CredentialProgressStage::Queued => {
            wire::CredentialProgressStage::CredentialProgressQueued as i32
        },
        CredentialProgressStage::Refreshing => {
            wire::CredentialProgressStage::CredentialProgressRefreshing as i32
        },
        CredentialProgressStage::Revoking => {
            wire::CredentialProgressStage::CredentialProgressRevoking as i32
        },
        CredentialProgressStage::Ready => {
            wire::CredentialProgressStage::CredentialProgressReady as i32
        },
        CredentialProgressStage::Failed => {
            wire::CredentialProgressStage::CredentialProgressFailed as i32
        },
    }
}

fn attachment_progress_stage(
    v: wire::AttachmentProgressStage,
) -> Result<AttachmentProgressStage, FromGrpcError> {
    match v {
        wire::AttachmentProgressStage::AttachmentQueued => Ok(AttachmentProgressStage::Queued),
        wire::AttachmentProgressStage::AttachmentStarting => Ok(AttachmentProgressStage::Starting),
        wire::AttachmentProgressStage::AttachmentStopping => Ok(AttachmentProgressStage::Stopping),
        wire::AttachmentProgressStage::AttachmentReady => Ok(AttachmentProgressStage::Ready),
        wire::AttachmentProgressStage::AttachmentFailed => Ok(AttachmentProgressStage::Failed),
        wire::AttachmentProgressStage::Unspecified => {
            Err(FromGrpcError::Unspecified("attachment progress stage"))
        },
    }
}

fn to_attachment_progress_stage(v: AttachmentProgressStage) -> i32 {
    match v {
        AttachmentProgressStage::Queued => wire::AttachmentProgressStage::AttachmentQueued as i32,
        AttachmentProgressStage::Starting => {
            wire::AttachmentProgressStage::AttachmentStarting as i32
        },
        AttachmentProgressStage::Stopping => {
            wire::AttachmentProgressStage::AttachmentStopping as i32
        },
        AttachmentProgressStage::Ready => wire::AttachmentProgressStage::AttachmentReady as i32,
        AttachmentProgressStage::Failed => wire::AttachmentProgressStage::AttachmentFailed as i32,
    }
}

fn progress_snapshot(v: &wire::ProgressSnapshot) -> Result<ProgressSnapshot, FromGrpcError> {
    resource_count(v.resources.len())?;
    resource_count(v.actions.len())?;
    Ok(ProgressSnapshot {
        desired_revision: ResourceRevision::new(v.desired_revision),
        observed_revision: v.observed_revision.map(ResourceRevision::new),
        resources: v
            .resources
            .iter()
            .map(resource_status)
            .collect::<Result<_, _>>()?,
        actions: v
            .actions
            .iter()
            .map(action_receipt)
            .collect::<Result<_, _>>()?,
    })
}

fn to_progress_snapshot(v: &ProgressSnapshot) -> wire::ProgressSnapshot {
    wire::ProgressSnapshot {
        desired_revision: v.desired_revision.get(),
        observed_revision: v.observed_revision.map(ResourceRevision::get),
        resources: v.resources.iter().map(to_resource_status).collect(),
        actions: v.actions.iter().map(to_action_receipt).collect(),
    }
}

fn provider_preparation_progress(
    v: &wire::ProviderPreparationProgress,
) -> Result<ProviderPreparationProgress, FromGrpcError> {
    resource_count(v.resource_names.len())?;
    Ok(ProviderPreparationProgress {
        digest: provider_id(&v.digest)?,
        catalog_name: v.catalog_name.clone(),
        resource_names: v
            .resource_names
            .iter()
            .cloned()
            .map(resource_name)
            .collect::<Result<_, _>>()?,
        stage: provider_preparation_stage(
            wire::ProviderPreparationStage::try_from(v.stage)
                .map_err(|_| FromGrpcError::Invalid("provider preparation stage"))?,
        )?,
        completed_bytes: v.completed_bytes,
        total_bytes: v.total_bytes,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        queued_digests: v.queued_digests,
        active_digests: v.active_digests,
        retry_count: v.retry_count,
    })
}

fn to_provider_preparation_progress(
    v: &ProviderPreparationProgress,
) -> wire::ProviderPreparationProgress {
    wire::ProviderPreparationProgress {
        digest: v.digest.as_bytes().to_vec().into(),
        catalog_name: v.catalog_name.clone(),
        resource_names: v.resource_names.iter().map(ToString::to_string).collect(),
        stage: to_provider_preparation_stage(v.stage),
        completed_bytes: v.completed_bytes,
        total_bytes: v.total_bytes,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        queued_digests: v.queued_digests,
        active_digests: v.active_digests,
        retry_count: v.retry_count,
    }
}

fn serving_progress(v: &wire::ServingProgress) -> Result<ServingProgress, FromGrpcError> {
    Ok(ServingProgress {
        revision: ResourceRevision::new(v.revision),
        stage: serving_progress_stage(
            wire::ServingProgressStage::try_from(v.stage)
                .map_err(|_| FromGrpcError::Invalid("serving progress stage"))?,
        )?,
        completed: v.completed,
        total: v.total,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        queued_generations: v.queued_generations,
        retry_count: v.retry_count,
    })
}

fn to_serving_progress(v: &ServingProgress) -> wire::ServingProgress {
    wire::ServingProgress {
        revision: v.revision.get(),
        stage: to_serving_progress_stage(v.stage),
        completed: v.completed,
        total: v.total,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        queued_generations: v.queued_generations,
        retry_count: v.retry_count,
    }
}

fn credential_progress(v: &wire::CredentialProgress) -> Result<CredentialProgress, FromGrpcError> {
    let key = resource_key(&req(v.key.clone(), "credential progress key")?)?;
    if key.kind != ResourceKind::Credential {
        return Err(FromGrpcError::Invalid("credential progress key"));
    }
    Ok(CredentialProgress {
        key,
        stage: credential_progress_stage(
            wire::CredentialProgressStage::try_from(v.stage)
                .map_err(|_| FromGrpcError::Invalid("credential progress stage"))?,
        )?,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        retry_count: v.retry_count,
    })
}

fn to_credential_progress(v: &CredentialProgress) -> wire::CredentialProgress {
    wire::CredentialProgress {
        key: Some(to_resource_key(&v.key)),
        stage: to_credential_progress_stage(v.stage),
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        retry_count: v.retry_count,
    }
}

fn attachment_progress(v: &wire::AttachmentProgress) -> Result<AttachmentProgress, FromGrpcError> {
    let key = resource_key(&req(v.key.clone(), "attachment progress key")?)?;
    if key.kind != ResourceKind::Attachment {
        return Err(FromGrpcError::Invalid("attachment progress key"));
    }
    Ok(AttachmentProgress {
        key,
        stage: attachment_progress_stage(
            wire::AttachmentProgressStage::try_from(v.stage)
                .map_err(|_| FromGrpcError::Invalid("attachment progress stage"))?,
        )?,
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        retry_count: v.retry_count,
    })
}

fn to_attachment_progress(v: &AttachmentProgress) -> wire::AttachmentProgress {
    wire::AttachmentProgress {
        key: Some(to_resource_key(&v.key)),
        stage: to_attachment_progress_stage(v.stage),
        error_code: v.error_code.clone(),
        detail: v.detail.clone(),
        retry_count: v.retry_count,
    }
}

pub fn progress_event(v: &wire::ProgressEvent) -> Result<ProgressEvent, FromGrpcError> {
    let event = match req(v.event.clone(), "progress event")? {
        wire::progress_event::Event::Snapshot(value) => {
            ProgressEventKind::Snapshot(progress_snapshot(&value)?)
        },
        wire::progress_event::Event::ResourcePhaseChanged(value) => {
            ProgressEventKind::ResourcePhaseChanged(resource_status(&req(
                value.status,
                "resource status",
            )?)?)
        },
        wire::progress_event::Event::ProviderPreparation(value) => {
            ProgressEventKind::ProviderPreparation(provider_preparation_progress(&value)?)
        },
        wire::progress_event::Event::ServingProgress(value) => {
            ProgressEventKind::ServingProgress(serving_progress(&value)?)
        },
        wire::progress_event::Event::CredentialProgress(value) => {
            ProgressEventKind::CredentialProgress(credential_progress(&value)?)
        },
        wire::progress_event::Event::AttachmentProgress(value) => {
            ProgressEventKind::AttachmentProgress(attachment_progress(&value)?)
        },
        wire::progress_event::Event::RevisionReady(value) => {
            ProgressEventKind::RevisionReady(ResourceRevision::new(value.revision))
        },
        wire::progress_event::Event::RevisionFailed(value) => ProgressEventKind::RevisionFailed {
            revision: ResourceRevision::new(value.revision),
            error_code: value.error_code,
            detail: value.detail,
        },
        wire::progress_event::Event::RevisionSuperseded(value) => {
            ProgressEventKind::RevisionSuperseded {
                revision: ResourceRevision::new(value.revision),
                replaced_by: ResourceRevision::new(value.replaced_by),
            }
        },
        wire::progress_event::Event::ActionCompleted(value) => ProgressEventKind::ActionCompleted(
            action_receipt(&req(value.receipt, "action receipt")?)?,
        ),
        wire::progress_event::Event::ActionFailed(value) => ProgressEventKind::ActionFailed {
            receipt: action_receipt(&req(value.receipt, "action receipt")?)?,
            error_code: value.error_code,
            detail: value.detail,
        },
        wire::progress_event::Event::Resync(value) => {
            ProgressEventKind::Resync(progress_snapshot(&value)?)
        },
    };
    Ok(ProgressEvent {
        daemon_instance_id: v.daemon_instance_id.clone(),
        sequence: v.sequence,
        target: progress_target(&req(v.target.clone(), "progress target")?)?,
        event,
    })
}

pub fn to_progress_event(v: &ProgressEvent) -> wire::ProgressEvent {
    let event = match &v.event {
        ProgressEventKind::Snapshot(value) => {
            wire::progress_event::Event::Snapshot(to_progress_snapshot(value))
        },
        ProgressEventKind::ResourcePhaseChanged(value) => {
            wire::progress_event::Event::ResourcePhaseChanged(wire::ResourcePhaseChanged {
                status: Some(to_resource_status(value)),
            })
        },
        ProgressEventKind::ProviderPreparation(value) => {
            wire::progress_event::Event::ProviderPreparation(to_provider_preparation_progress(
                value,
            ))
        },
        ProgressEventKind::ServingProgress(value) => {
            wire::progress_event::Event::ServingProgress(to_serving_progress(value))
        },
        ProgressEventKind::CredentialProgress(value) => {
            wire::progress_event::Event::CredentialProgress(to_credential_progress(value))
        },
        ProgressEventKind::AttachmentProgress(value) => {
            wire::progress_event::Event::AttachmentProgress(to_attachment_progress(value))
        },
        ProgressEventKind::RevisionReady(value) => {
            wire::progress_event::Event::RevisionReady(wire::RevisionReady {
                revision: value.get(),
            })
        },
        ProgressEventKind::RevisionFailed {
            revision,
            error_code,
            detail,
        } => wire::progress_event::Event::RevisionFailed(wire::RevisionFailed {
            revision: revision.get(),
            error_code: error_code.clone(),
            detail: detail.clone(),
        }),
        ProgressEventKind::RevisionSuperseded {
            revision,
            replaced_by,
        } => wire::progress_event::Event::RevisionSuperseded(wire::RevisionSuperseded {
            revision: revision.get(),
            replaced_by: replaced_by.get(),
        }),
        ProgressEventKind::ActionCompleted(value) => {
            wire::progress_event::Event::ActionCompleted(wire::ActionCompleted {
                receipt: Some(to_action_receipt(value)),
            })
        },
        ProgressEventKind::ActionFailed {
            receipt,
            error_code,
            detail,
        } => wire::progress_event::Event::ActionFailed(wire::ActionFailed {
            receipt: Some(to_action_receipt(receipt)),
            error_code: error_code.clone(),
            detail: detail.clone(),
        }),
        ProgressEventKind::Resync(value) => {
            wire::progress_event::Event::Resync(to_progress_snapshot(value))
        },
    };
    wire::ProgressEvent {
        daemon_instance_id: v.daemon_instance_id.clone(),
        sequence: v.sequence,
        target: Some(to_progress_target(v.target)),
        event: Some(event),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_mutation_round_trips_and_rejects_short_id() {
        let value = ActiveMutation {
            mutation_id: MutationId::from_bytes([2; 16]),
            lease_deadline_unix_ms: 1_700_000_000_000,
        };
        assert_eq!(active_mutation(&to_active_mutation(&value)).unwrap(), value);

        let mut short = to_active_mutation(&value);
        short.mutation_id = vec![1].into();
        assert!(matches!(
            active_mutation(&short),
            Err(FromGrpcError::Length { .. })
        ));
    }
    #[test]
    fn mutation_op_round_trips_mount_and_credential_variants() {
        let create = MutationOp::CreateMount(MountDefinition {
            name: MountName::new("demo").unwrap(),
            provider: ProviderId::from_wasm_bytes(b"demo"),
            auth: None,
            limits: None,
            config: br"{}".to_vec(),
        });
        match mutation_op(&to_mutation_op(&create)).unwrap() {
            MutationOp::CreateMount(definition) => assert_eq!(definition.name.as_str(), "demo"),
            other => panic!("wrong op: {other:?}"),
        }

        let update = MutationOp::UpdateMount {
            name: MountName::new("demo").unwrap(),
            patch: MountPatch::default(),
        };
        match mutation_op(&to_mutation_op(&update)).unwrap() {
            MutationOp::UpdateMount { name, .. } => assert_eq!(name.as_str(), "demo"),
            other => panic!("wrong op: {other:?}"),
        }

        let remove = MutationOp::RemoveMount {
            name: MountName::new("demo").unwrap(),
        };
        match mutation_op(&to_mutation_op(&remove)).unwrap() {
            MutationOp::RemoveMount { name } => assert_eq!(name.as_str(), "demo"),
            other => panic!("wrong op: {other:?}"),
        }

        let submit = MutationOp::SubmitCredential(CredentialSubmission {
            provider: ProviderId::from_wasm_bytes(b"provider"),
            scheme: "oauth".into(),
            account_label: "default".into(),
            material: CredentialMaterial::StaticToken {
                token: SecretBytes::new(b"secret".to_vec()),
            },
            overrides: CredentialClientOverrides {
                client_id: None,
                client_secret: None,
                redirect_uri: None,
                scopes: None,
            },
        });
        match mutation_op(&to_mutation_op(&submit)).unwrap() {
            MutationOp::SubmitCredential(submission) => {
                assert_eq!(submission.account_label, "default");
            },
            other => panic!("wrong op: {other:?}"),
        }

        let key = CredentialKey {
            provider_name: "provider".into(),
            scheme: "oauth".into(),
            account_label: "default".into(),
        };
        let delete = MutationOp::DeleteCredential(key.clone());
        assert!(matches!(
            mutation_op(&to_mutation_op(&delete)).unwrap(),
            MutationOp::DeleteCredential(k) if k == key
        ));
        let revoke = MutationOp::RevokeCredential(key.clone());
        assert!(matches!(
            mutation_op(&to_mutation_op(&revoke)).unwrap(),
            MutationOp::RevokeCredential(k) if k == key
        ));
    }
    #[test]
    fn mutation_op_result_round_trips_mount_and_credential() {
        let mount = MutationOpResult::Mount(MountOpResult {
            name: MountName::new("demo").unwrap(),
            version: Some(MountVersion::from_digest([3; 32])),
            revision: MountRevision::new(4),
        });
        assert_eq!(
            mutation_op_result(&to_mutation_op_result(&mount)).unwrap(),
            mount
        );

        let credential = MutationOpResult::Credential(CredentialStatus {
            key: CredentialKey {
                provider_name: "provider".into(),
                scheme: "oauth".into(),
                account_label: "default".into(),
            },
            provider: ProviderId::from_wasm_bytes(b"provider"),
            kind: CredentialKind::OAuth,
            scopes: vec!["read".into()],
            auth_fingerprint: AuthRuntimeFingerprint::from_digest([5; 32]),
            version: CredentialVersion::new(std::num::NonZeroU64::new(1).unwrap()),
            generation: CredentialGeneration::new(std::num::NonZeroU64::new(2).unwrap()),
            status: CredentialStatusKind::Active,
            last_mutation_id: MutationId::from_bytes([6; 16]),
        });
        assert_eq!(
            mutation_op_result(&to_mutation_op_result(&credential)).unwrap(),
            credential
        );
    }
    #[test]
    fn serving_outcome_round_trips_with_and_without_detail() {
        let serving = ServingOutcome {
            serving: true,
            recovery_detail: None,
        };
        assert_eq!(serving_outcome(&to_serving_outcome(&serving)), serving);

        let recovering = ServingOutcome {
            serving: false,
            recovery_detail: Some("store unavailable".into()),
        };
        assert_eq!(
            serving_outcome(&to_serving_outcome(&recovering)),
            recovering
        );
    }
    #[test]
    fn mount_patch_preserves_keep_set_clear() {
        let keep = wire::MountPatch {
            provider: None,
            auth: Some(wire::MountCredentialPatch {
                value: Some(wire::mount_credential_patch::Value::Keep(wire::Empty {})),
            }),
            limits: Some(wire::MountLimitsPatch {
                value: Some(wire::mount_limits_patch::Value::Clear(wire::Empty {})),
            }),
            config: Some(wire::BytesPatch {
                value: Some(wire::bytes_patch::Value::Set(
                    br#"{"enabled":true}"#.to_vec().into(),
                )),
            }),
        };
        let patch = mount_patch(&keep).unwrap();
        assert!(matches!(patch.auth, MountField::Keep));
        assert!(matches!(patch.limits, MountField::Clear));
        assert!(
            matches!(patch.config, MountField::Set(ref bytes) if bytes == br#"{"enabled":true}"#)
        );
    }
    #[test]
    fn mount_patch_encoder_round_trips_and_validates_json() {
        let value = MountPatch {
            provider: Some(ProviderId::from_wasm_bytes(b"provider")),
            auth: MountField::Set(MountCredential {
                scheme: "oauth".into(),
                account_label: "default".into(),
            }),
            limits: MountField::Clear,
            config: MountField::Set(br#"{"enabled":true}"#.to_vec()),
        };
        assert_eq!(mount_patch(&to_mount_patch(&value)).unwrap(), value);
        let invalid = wire::MountPatch {
            config: Some(wire::BytesPatch {
                value: Some(wire::bytes_patch::Value::Set(b"not-json".to_vec().into())),
            }),
            ..Default::default()
        };
        assert!(matches!(mount_patch(&invalid), Err(FromGrpcError::Json(_))));
    }
    #[test]
    fn secret_domain_debug_is_redacted() {
        let value = CredentialSubmission {
            provider: ProviderId::from_wasm_bytes(b"x"),
            scheme: "oauth".into(),
            account_label: "a".into(),
            material: CredentialMaterial::OAuth {
                access_token: SecretBytes::new(b"access-secret".to_vec()),
                refresh_token: Some(SecretBytes::new(b"refresh-secret".to_vec())),
                expires_at_unix: None,
                token_type: "Bearer".into(),
                scopes: vec![],
                upstream_identity: None,
            },
            overrides: CredentialClientOverrides {
                client_id: None,
                client_secret: Some(SecretBytes::new(b"client-secret".to_vec())),
                redirect_uri: None,
                scopes: Some(vec![]),
            },
        };
        let debug = format!("{value:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
        assert!(!debug.contains("client-secret"));
    }
    #[test]
    fn credential_scope_presence_survives_wire_round_trip() {
        let absent = credential_overrides(&wire::CredentialClientOverrides::default());
        assert_eq!(absent.scopes, None);
        let present = CredentialClientOverrides {
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            scopes: Some(vec![]),
        };
        let wire = to_credential_overrides(&present);
        assert!(wire.scopes.is_some());
        assert_eq!(credential_overrides(&wire).scopes, Some(vec![]));
    }
    #[test]
    fn credential_submission_and_status_round_trip_without_secret_loss() {
        let submission = CredentialSubmission {
            provider: ProviderId::from_wasm_bytes(b"provider"),
            scheme: "oauth".into(),
            account_label: "default".into(),
            material: CredentialMaterial::OAuth {
                access_token: SecretBytes::new(b"access".to_vec()),
                refresh_token: Some(SecretBytes::new(b"refresh".to_vec())),
                expires_at_unix: Some(123),
                token_type: "Bearer".into(),
                scopes: vec!["read".into()],
                upstream_identity: Some("user".into()),
            },
            overrides: CredentialClientOverrides {
                client_id: Some("client".into()),
                client_secret: Some(SecretBytes::new(b"client-secret".to_vec())),
                redirect_uri: None,
                scopes: Some(vec![]),
            },
        };
        let decoded = credential_submission(&to_credential_submission(&submission)).unwrap();
        match decoded.material {
            CredentialMaterial::OAuth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token.expose(), b"access");
                assert_eq!(refresh_token.unwrap().expose(), b"refresh");
            },
            CredentialMaterial::StaticToken { .. } => panic!("wrong material"),
        }
        assert_eq!(decoded.overrides.scopes, Some(vec![]));

        let status = CredentialStatus {
            key: CredentialKey {
                provider_name: "provider".into(),
                scheme: "oauth".into(),
                account_label: "default".into(),
            },
            provider: submission.provider,
            kind: CredentialKind::OAuth,
            scopes: vec!["read".into()],
            auth_fingerprint: AuthRuntimeFingerprint::from_digest([5; 32]),
            version: CredentialVersion::new(std::num::NonZeroU64::new(1).unwrap()),
            generation: CredentialGeneration::new(std::num::NonZeroU64::new(2).unwrap()),
            status: CredentialStatusKind::Active,
            last_mutation_id: MutationId::from_bytes([6; 16]),
        };
        assert_eq!(
            credential_status(&to_credential_status(&status)).unwrap(),
            status
        );
    }
    #[test]
    fn provider_upload_and_import_receipts_round_trip() {
        let digest = ProviderId::from_wasm_bytes(b"provider");
        let start = to_provider_upload_start("provider.wasm", 42, &digest);
        assert_eq!(start.file_name, "provider.wasm");
        assert_eq!(start.total_length, 42);
        assert_eq!(start.digest, digest.as_bytes().to_vec());

        let receipt = ProviderImportReceipt {
            provider: ProviderReference {
                id: digest,
                name: "provider".into(),
                version: Some("1".into()),
            },
            disposition: ProviderImportDisposition::Inserted,
        };
        assert_eq!(
            provider_import_receipt(&to_provider_import_receipt(&receipt)).unwrap(),
            receipt
        );
    }
    #[test]
    fn provider_availability_is_strict() {
        let metadata = ProviderMetadata {
            reference: ProviderReference {
                id: ProviderId::from_wasm_bytes(b"provider"),
                name: "provider".into(),
                version: None,
            },
            manifest: br"{}".to_vec(),
        };
        let unavailable = wire::ProviderEntry {
            metadata: Some(to_provider_metadata(&metadata)),
            embedded: false,
            retained: false,
        };
        assert!(matches!(
            provider_entry(&unavailable),
            Err(FromGrpcError::Invalid("provider availability"))
        ));
    }
    #[test]
    fn daemon_status_and_inventory_round_trip_nested_state() {
        let report = |state| HealthReport::new(state, "ok");
        let health = DaemonHealth::new(
            report(HealthState::Healthy),
            report(HealthState::Healthy),
            report(HealthState::Degraded),
        );
        let info = DaemonInfo {
            version: "0.2.1".into(),
            pid: 42,
            instance_id: "instance".into(),
            executable: PathBuf::from("/bin/omnifs"),
            attach_unix: Some(PathBuf::from("/tmp/omnifs.sock")),
            attach_tcp: Some("127.0.0.1:1234".parse().unwrap()),
        };
        let active_mutation = ActiveMutation {
            mutation_id: MutationId::from_bytes([7; 16]),
            lease_deadline_unix_ms: 1_700_000_000_000,
        };
        let status = DaemonStatus {
            version: info.version.clone(),
            pid: info.pid,
            instance_id: info.instance_id.clone(),
            executable: info.executable.clone(),
            attach_tcp: info.attach_tcp,
            filesystems: vec![],
            mounts: vec![crate::MountInfo {
                mount: "demo".into(),
                provider_name: "provider".into(),
                provider_id: "hash".into(),
                auth_health: Some(CredentialHealth::Ready),
            }],
            health: Box::new(health.clone()),
            active_mutation: Some(active_mutation),
        };
        let decoded_status = daemon_status(&to_daemon_status(&status)).unwrap();
        assert_eq!(decoded_status.mounts.len(), 1);
        assert_eq!(decoded_status.mounts[0].mount, "demo");
        assert_eq!(
            decoded_status.mounts[0].auth_health,
            Some(CredentialHealth::Ready)
        );
        assert_eq!(decoded_status.active_mutation, Some(active_mutation));

        let mount_record = MountRecord {
            definition: MountDefinition {
                name: MountName::new("demo").unwrap(),
                provider: ProviderId::from_wasm_bytes(b"demo"),
                auth: None,
                limits: None,
                config: br"{}".to_vec(),
            },
            provider: ProviderReference {
                id: ProviderId::from_wasm_bytes(b"demo"),
                name: "demo".into(),
                version: None,
            },
            version: MountVersion::from_digest([1; 32]),
            revision: MountRevision::new(1),
            health: MountHealth::Active,
            auth_health: None,
            last_mutation_id: MutationId::from_bytes([9; 16]),
        };
        let inventory = DaemonInventory {
            info,
            phase: DaemonPhase::Ready,
            durable_revision: Some(MountRevision::new(3)),
            serving_revision: Some(MountRevision::new(2)),
            health,
            mounts: vec![mount_record.clone()],
            credentials: vec![],
            attachments: vec![],
            active_mutation: Some(active_mutation),
        };
        let decoded = daemon_inventory(&to_daemon_inventory(&inventory)).unwrap();
        assert_eq!(decoded.phase, inventory.phase);
        assert_eq!(decoded.durable_revision, inventory.durable_revision);
        assert_eq!(decoded.info.attach_tcp, inventory.info.attach_tcp);
        assert_eq!(decoded.mounts, vec![mount_record]);
        assert_eq!(decoded.active_mutation, Some(active_mutation));
    }
    #[test]
    fn lookup_replies_use_message_presence_as_the_only_absence_signal() {
        let provider = wire::GetProviderMetadataResponse { metadata: None };
        let mount = wire::GetMountResponse { mount: None };
        let credential = wire::GetCredentialStatusResponse { status: None };
        assert!(provider.metadata.is_none());
        assert!(mount.mount.is_none());
        assert!(credential.status.is_none());
    }
    #[cfg(unix)]
    #[test]
    fn unix_paths_round_trip_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let location = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', b't', 0x80]));
        let spec = omnifs_core::fs::Spec::new(
            omnifs_core::fs::Id::new("x").unwrap(),
            omnifs_core::fs::Protocol::Fuse,
            omnifs_core::fs::Runtime::Host,
            location,
        )
        .unwrap();
        assert_eq!(
            filesystem_spec(&to_filesystem_spec(&spec))
                .unwrap()
                .location(),
            spec.location()
        );
    }

    fn new_resource_name(value: &str) -> ResourceName {
        ResourceName::new(value).unwrap()
    }

    fn supported_attachment() -> AttachmentDefinition {
        let (protocol, runtime) = if cfg!(target_os = "linux") {
            (
                omnifs_core::fs::Protocol::Fuse,
                omnifs_core::fs::Runtime::Host,
            )
        } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            (
                omnifs_core::fs::Protocol::Fuse,
                omnifs_core::fs::Runtime::Libkrun,
            )
        } else {
            (
                omnifs_core::fs::Protocol::Nfs,
                omnifs_core::fs::Runtime::Host,
            )
        };
        let location = if runtime == omnifs_core::fs::Runtime::Host {
            PathBuf::from("/tmp/omnifs")
        } else {
            PathBuf::from(omnifs_core::fs::GUEST_LOCATION)
        };
        AttachmentDefinition {
            name: new_resource_name("local"),
            spec: AttachmentSpec::new(protocol, runtime, location, None, None).unwrap(),
        }
    }

    fn declarations() -> ResourceDeclarations {
        ResourceDeclarations {
            api_version: crate::API_VERSION.into(),
            resources: vec![
                ResourceDefinition::Provider(ProviderDefinition {
                    name: new_resource_name("github"),
                    artifact: ProviderId::from_wasm_bytes(b"github"),
                }),
                ResourceDefinition::Credential(CredentialDefinition {
                    name: new_resource_name("github-default"),
                    provider: new_resource_name("github"),
                    scheme: "oauth".into(),
                    account: "default".into(),
                }),
                ResourceDefinition::Mount(MountResourceDefinition {
                    name: new_resource_name("github-mount"),
                    provider: new_resource_name("github"),
                    credential: Some(new_resource_name("github-default")),
                    config: serde_json::json!({"enabled": true}),
                    limits: Some(ResourceLimits {
                        max_memory_mb: Some(64),
                        max_fetch_blob_bytes: Some(1024),
                    }),
                }),
                ResourceDefinition::Attachment(supported_attachment()),
            ],
        }
    }

    fn action_receipt_for(kind: ActionKind) -> ActionReceipt {
        let target = match kind {
            ActionKind::RestartAttachment => {
                ResourceKey::new(ResourceKind::Attachment, new_resource_name("local"))
            },
            ActionKind::SetCredentialMaterial | ActionKind::RevokeCredential => ResourceKey::new(
                ResourceKind::Credential,
                new_resource_name("github-default"),
            ),
        };
        ActionReceipt {
            action_id: ActionId::from_bytes([7; 16]),
            kind,
            target,
            action_generation: 2,
            phase: ActionPhase::Accepted,
        }
    }

    #[test]
    fn typed_resources_round_trip_and_reject_malformed_input() {
        let declarations = declarations();
        let decoded = resource_declarations(&to_resource_declarations(&declarations)).unwrap();
        assert_eq!(
            decoded.normalize().unwrap(),
            declarations.clone().normalize().unwrap()
        );
        let normalized = declarations.clone().normalize().unwrap();
        let snapshot = ResourceSnapshot {
            revision: ResourceRevision::new(1),
            desired_digest: normalized.digest(),
            resources: normalized.resources().to_vec(),
            resource_statuses: vec![],
        };
        assert_eq!(
            get_resources_response(&to_get_resources_response(&snapshot)).unwrap(),
            snapshot
        );

        let mut duplicate = to_resource_declarations(&declarations);
        duplicate.resources.push(duplicate.resources[0].clone());
        assert!(matches!(
            resource_declarations(&duplicate),
            Err(FromGrpcError::Invalid("resource declarations"))
        ));
        assert!(matches!(
            resource_definition(&wire::ResourceDefinition::default()),
            Err(FromGrpcError::Missing("resource definition"))
        ));

        let mut oversized = wire::ResourceDeclarations {
            api_version: crate::API_VERSION.into(),
            resources: Vec::new(),
        };
        oversized.resources.resize_with(
            CONTROL_RESOURCE_MAX_COUNT + 1,
            wire::ResourceDefinition::default,
        );
        assert!(matches!(
            resource_declarations(&oversized),
            Err(FromGrpcError::TooManyResources { .. })
        ));
        let unsupported = wire::ResourceDeclarations {
            api_version: "omnifs.dev/v999".into(),
            resources: Vec::new(),
        };
        assert!(matches!(
            resource_declarations(&unsupported),
            Err(FromGrpcError::UnsupportedApiVersion(version))
                if version == "omnifs.dev/v999"
        ));

        let bad_path = wire::AttachmentSpec {
            protocol: wire::FsProtocol::FsFuse as i32,
            runtime: wire::FsRuntime::FsHost as i32,
            location: b"relative".to_vec().into(),
            docker_image: None,
            libkrun_guest_image: None,
        };
        assert!(matches!(
            attachment_spec(&bad_path),
            Err(FromGrpcError::Invalid("attachment spec"))
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one typed request and receipt matrix"
    )]
    fn typed_plan_apply_and_actions_round_trip_with_exact_ids() {
        let declarations = declarations();
        let normalized = declarations.clone().normalize().unwrap();
        let plan = ResourcePlan {
            base_revision: ResourceRevision::new(4),
            desired_digest: normalized.digest(),
            normalized: normalized.resources().to_vec(),
            changes: vec![],
        };
        assert_eq!(resource_plan(&to_resource_plan(&plan)).unwrap(), plan);
        assert_eq!(
            plan_resources_response(&to_plan_resources_response(&plan)).unwrap(),
            plan
        );

        let request = ApplyResourcesRequest {
            mutation_id: MutationId::from_bytes([1; 16]),
            base_revision: ResourceRevision::new(4),
            expected_desired_digest: normalized.digest(),
            declarations,
            credential_material: vec![CredentialMaterialSidecar {
                credential: new_resource_name("github-default"),
                material: CredentialMaterial::StaticToken {
                    token: SecretBytes::new(b"private-material".to_vec()),
                },
                overrides: CredentialClientOverrides {
                    client_id: None,
                    client_secret: None,
                    redirect_uri: None,
                    scopes: None,
                },
            }],
        };
        let decoded = apply_resources_request(&to_apply_resources_request(&request)).unwrap();
        assert!(!format!("{request:?}").contains("private-material"));
        assert_eq!(decoded.mutation_id, request.mutation_id);
        assert_eq!(
            decoded.expected_desired_digest,
            request.expected_desired_digest
        );
        assert_eq!(decoded.credential_material.len(), 1);
        assert_eq!(
            plan_resources_request(&to_plan_resources_request(&request.declarations)).unwrap(),
            request.declarations
        );

        let apply_receipt = ApplyReceipt {
            mutation_id: request.mutation_id,
            revision: ResourceRevision::new(5),
            desired_digest: request.expected_desired_digest,
            created: 1,
            updated: 0,
            deleted: 0,
            changed: true,
        };
        assert_eq!(
            apply_resources_response(&to_apply_resources_response(&apply_receipt)).unwrap(),
            apply_receipt
        );

        let mut malformed = to_apply_resources_request(&request);
        malformed.expected_desired_digest = vec![1].into();
        assert!(matches!(
            apply_resources_request(&malformed),
            Err(FromGrpcError::Length {
                kind: "resource digest",
                ..
            })
        ));

        for kind in [
            ActionKind::SetCredentialMaterial,
            ActionKind::RevokeCredential,
            ActionKind::RestartAttachment,
        ] {
            let receipt = action_receipt_for(kind);
            assert_eq!(
                action_receipt(&to_action_receipt(&receipt)).unwrap(),
                receipt
            );
        }
        let credential_receipt = CredentialReceipt {
            action: action_receipt_for(ActionKind::SetCredentialMaterial),
            status: CredentialStatusKind::PendingRepublish,
        };
        assert_eq!(
            set_credential_material_response(&to_set_credential_material_response(
                &credential_receipt,
            ))
            .unwrap(),
            credential_receipt
        );
        assert_eq!(
            revoke_credential_response(&to_revoke_credential_response(&credential_receipt))
                .unwrap(),
            credential_receipt
        );
        let set_request = SetCredentialMaterialRequest {
            action_id: ActionId::from_bytes([8; 16]),
            base_action_generation: 2,
            credential: new_resource_name("github-default"),
            material: CredentialMaterial::StaticToken {
                token: SecretBytes::new(b"secret".to_vec()),
            },
            overrides: CredentialClientOverrides {
                client_id: None,
                client_secret: None,
                redirect_uri: None,
                scopes: None,
            },
        };
        let decoded_set =
            set_credential_material_request(&to_set_credential_material_request(&set_request))
                .unwrap();
        assert_eq!(decoded_set.action_id, set_request.action_id);
        assert_eq!(decoded_set.credential, set_request.credential);
        let revoke_request = RevokeCredentialRequest {
            action_id: ActionId::from_bytes([9; 16]),
            base_action_generation: 2,
            credential: new_resource_name("github-default"),
        };
        assert_eq!(
            revoke_credential_request(&to_revoke_credential_request(&revoke_request)).unwrap(),
            revoke_request
        );
        for code in [
            ControlErrorCode::UnsupportedApiVersion,
            ControlErrorCode::InvalidResource,
            ControlErrorCode::StaleBaseRevision,
            ControlErrorCode::DesiredDigestMismatch,
            ControlErrorCode::MutationIdReuseMismatch,
            ControlErrorCode::MissingProviderArtifact,
            ControlErrorCode::PlanTooLarge,
            ControlErrorCode::ActionUnavailable,
            ControlErrorCode::ActionIdReuseMismatch,
        ] {
            let error = ControlError::new(code, "safe");
            assert_eq!(error_detail(&to_error_detail(&error)).unwrap().code, code);
        }
        let mut short = to_action_receipt(&action_receipt_for(ActionKind::RevokeCredential));
        short.action_id = vec![1].into();
        assert!(matches!(
            action_receipt(&short),
            Err(FromGrpcError::Length { .. })
        ));
        let mut zero = to_action_receipt(&action_receipt_for(ActionKind::RevokeCredential));
        zero.action_generation = 0;
        assert!(matches!(
            action_receipt(&zero),
            Err(FromGrpcError::Zero("action generation"))
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one exhaustive progress variant matrix"
    )]
    fn every_progress_variant_round_trips_and_rejects_missing_or_unknown_variants() {
        let status = ResourceStatus {
            key: ResourceKey::new(
                ResourceKind::Credential,
                new_resource_name("github-default"),
            ),
            desired_revision: ResourceRevision::new(4),
            observed_revision: Some(ResourceRevision::new(3)),
            phase: ResourcePhase::Preparing,
            error_code: None,
            detail: None,
        };
        let snapshot = ProgressSnapshot {
            desired_revision: ResourceRevision::new(4),
            observed_revision: Some(ResourceRevision::new(3)),
            resources: vec![status.clone()],
            actions: vec![action_receipt_for(ActionKind::SetCredentialMaterial)],
        };
        let attachment_key = ResourceKey::new(ResourceKind::Attachment, new_resource_name("local"));
        let events = vec![
            ProgressEventKind::Snapshot(snapshot.clone()),
            ProgressEventKind::ResourcePhaseChanged(status.clone()),
            ProgressEventKind::ProviderPreparation(ProviderPreparationProgress {
                digest: ProviderId::from_wasm_bytes(b"github"),
                catalog_name: "github".into(),
                resource_names: vec![new_resource_name("github")],
                stage: ProviderPreparationStage::Compiling,
                completed_bytes: 4,
                total_bytes: Some(8),
                error_code: None,
                detail: None,
                queued_digests: 3,
                active_digests: 2,
                retry_count: 1,
            }),
            ProgressEventKind::ServingProgress(ServingProgress {
                revision: ResourceRevision::new(4),
                stage: ServingProgressStage::Building,
                completed: 1,
                total: 2,
                error_code: None,
                detail: None,
                queued_generations: 1,
                retry_count: 2,
            }),
            ProgressEventKind::CredentialProgress(CredentialProgress {
                key: status.key.clone(),
                stage: CredentialProgressStage::Refreshing,
                error_code: None,
                detail: None,
                retry_count: 3,
            }),
            ProgressEventKind::AttachmentProgress(AttachmentProgress {
                key: attachment_key,
                stage: AttachmentProgressStage::Starting,
                error_code: None,
                detail: None,
                retry_count: 4,
            }),
            ProgressEventKind::RevisionReady(ResourceRevision::new(4)),
            ProgressEventKind::RevisionFailed {
                revision: ResourceRevision::new(4),
                error_code: "failed".into(),
                detail: "safe".into(),
            },
            ProgressEventKind::RevisionSuperseded {
                revision: ResourceRevision::new(4),
                replaced_by: ResourceRevision::new(5),
            },
            ProgressEventKind::ActionCompleted(action_receipt_for(ActionKind::RevokeCredential)),
            ProgressEventKind::ActionFailed {
                receipt: action_receipt_for(ActionKind::RestartAttachment),
                error_code: "failed".into(),
                detail: "safe".into(),
            },
            ProgressEventKind::Resync(snapshot),
        ];
        for event in events {
            let value = ProgressEvent {
                daemon_instance_id: "daemon".into(),
                sequence: 1,
                target: ProgressTarget::Current,
                event,
            };
            assert_eq!(progress_event(&to_progress_event(&value)).unwrap(), value);
        }
        assert!(matches!(
            progress_target(&wire::WatchProgressRequest::default()),
            Err(FromGrpcError::Missing("progress target"))
        ));
        let invalid_status = wire::ResourceStatus {
            key: Some(to_resource_key(&status.key)),
            desired_revision: 1,
            observed_revision: None,
            phase: 999,
            error_code: None,
            detail: None,
        };
        assert!(matches!(
            resource_status(&invalid_status),
            Err(FromGrpcError::Invalid("resource phase"))
        ));
    }
}
