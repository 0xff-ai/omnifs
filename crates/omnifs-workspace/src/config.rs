//! Workspace-owned configuration and durable frontend planning.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::io::{ensure_private_dir, write_atomic};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: System,
    pub telemetry: Telemetry,
    pub frontend: FrontendAssets,
    pub frontends: FrontendPlan,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct System {
    pub frontend_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Telemetry {
    pub enabled: bool,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrontendAssets {
    pub guest_image: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrontendPlan {
    entries: Vec<FrontendSpec>,
    configured: bool,
}

impl Serialize for FrontendPlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.entries.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FrontendPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            entries: Vec::<FrontendSpec>::deserialize(deserializer)?,
            configured: true,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendSpec {
    pub filesystem: Filesystem,
    pub environment: Environment,
    pub location: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filesystem {
    Fuse,
    Nfs,
}

impl Filesystem {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fuse => "fuse",
            Self::Nfs => "nfs",
        }
    }
}

impl fmt::Display for Filesystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Host,
    Docker,
    Krunkit,
}

impl Environment {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Krunkit => "krunkit",
        }
    }
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSource {
    PlatformDefault,
    Configured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostOs {
    Linux,
    MacOs,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FrontendId {
    filesystem: Filesystem,
    environment: Environment,
    location: Option<PathBuf>,
}

impl FrontendId {
    pub const fn filesystem(&self) -> Filesystem {
        self.filesystem
    }

    pub const fn environment(&self) -> Environment {
        self.environment
    }

    pub fn location(&self) -> Option<&Path> {
        self.location.as_deref()
    }
}

impl fmt::Display for FrontendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.filesystem, self.environment)?;
        if let Some(location) = &self.location {
            write!(f, ":{}", location.display())?;
        }
        Ok(())
    }
}

impl Serialize for FrontendId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveFrontend {
    pub filesystem: Filesystem,
    pub environment: Environment,
    pub location: Option<PathBuf>,
    pub source: PlanSource,
}

impl EffectiveFrontend {
    pub fn id(&self) -> FrontendId {
        FrontendId {
            filesystem: self.filesystem,
            environment: self.environment,
            location: self.location.clone(),
        }
    }
}

impl FrontendSpec {
    pub fn validate(&self, host_os: HostOs) -> Result<(), ConfigError> {
        match self.environment {
            Environment::Host => {
                if self.filesystem == Filesystem::Fuse && host_os != HostOs::Linux {
                    return Err(ConfigError::Validation(
                        "a host fuse frontend requires a Linux host".into(),
                    ));
                }
                if let Some(location) = &self.location
                    && !location.is_absolute()
                {
                    return Err(ConfigError::Validation(format!(
                        "host frontend location must be absolute: {}",
                        location.display()
                    )));
                }
            },
            Environment::Docker | Environment::Krunkit => {
                if self.filesystem != Filesystem::Fuse {
                    return Err(ConfigError::Validation(format!(
                        "the {} environment only delivers a fuse frontend",
                        self.environment
                    )));
                }
                if self.location.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "the {} environment owns its mount; location is not allowed",
                        self.environment
                    )));
                }
            },
        }
        Ok(())
    }
}

impl FrontendPlan {
    pub fn effective(
        &self,
        host_os: HostOs,
        default_location: impl AsRef<Path>,
    ) -> Result<Vec<EffectiveFrontend>, ConfigError> {
        let default_location = default_location.as_ref();
        if !default_location.is_absolute() {
            return Err(ConfigError::Validation(format!(
                "default frontend location must be absolute: {}",
                default_location.display()
            )));
        }
        let (specs, source) = if self.configured {
            (self.entries.clone(), PlanSource::Configured)
        } else {
            (
                Self::defaults(host_os, default_location),
                PlanSource::PlatformDefault,
            )
        };
        let mut effective = Vec::with_capacity(specs.len());
        let mut host_locations = Vec::new();
        let mut docker_seen = false;
        let mut krunkit_seen = false;
        for spec in specs {
            spec.validate(host_os)?;
            let location = match spec.environment {
                Environment::Host => Some(
                    spec.location
                        .unwrap_or_else(|| default_location.to_path_buf()),
                ),
                Environment::Docker | Environment::Krunkit => None,
            };
            match spec.environment {
                Environment::Docker if docker_seen => {
                    return Err(ConfigError::Validation(
                        "at most one docker frontend entry is allowed".into(),
                    ));
                },
                Environment::Docker => docker_seen = true,
                Environment::Krunkit if krunkit_seen => {
                    return Err(ConfigError::Validation(
                        "at most one krunkit frontend entry is allowed".into(),
                    ));
                },
                Environment::Krunkit => krunkit_seen = true,
                Environment::Host => {
                    let resolved = location.as_ref().ok_or_else(|| {
                        ConfigError::Validation("host frontend location did not resolve".into())
                    })?;
                    if host_locations.iter().any(|existing| existing == resolved) {
                        return Err(ConfigError::Validation(format!(
                            "two host frontend entries resolve to the same location {}",
                            resolved.display()
                        )));
                    }
                    host_locations.push(resolved.clone());
                },
            }
            effective.push(EffectiveFrontend {
                filesystem: spec.filesystem,
                environment: spec.environment,
                location,
                source,
            });
        }
        Ok(effective)
    }

    pub fn enable(
        &mut self,
        spec: FrontendSpec,
        host_os: HostOs,
        default_location: impl AsRef<Path>,
    ) -> Result<bool, ConfigError> {
        let mut candidate = self.clone();
        candidate.materialize(host_os, default_location.as_ref());
        let wanted = FrontendPlan {
            entries: vec![spec.clone()],
            configured: true,
        }
        .effective(host_os, default_location.as_ref())?
        .into_iter()
        .next()
        .ok_or_else(|| ConfigError::Validation("frontend plan entry disappeared".into()))?
        .id();
        if candidate
            .effective(host_os, default_location.as_ref())?
            .iter()
            .any(|existing| existing.id() == wanted)
        {
            *self = candidate;
            return Ok(false);
        }
        candidate.entries.push(spec);
        candidate.effective(host_os, default_location)?;
        *self = candidate;
        Ok(true)
    }

    pub fn disable(
        &mut self,
        id: &FrontendId,
        host_os: HostOs,
        default_location: impl AsRef<Path>,
    ) -> Result<bool, ConfigError> {
        let mut candidate = self.clone();
        candidate.materialize(host_os, default_location.as_ref());
        let before = candidate.entries.len();
        let effective = candidate.effective(host_os, default_location.as_ref())?;
        candidate.entries = candidate
            .entries
            .into_iter()
            .zip(effective)
            .filter(|(_, effective)| effective.id() != *id)
            .map(|(spec, _)| spec)
            .collect();
        let changed = candidate.entries.len() != before;
        candidate.effective(host_os, default_location)?;
        *self = candidate;
        Ok(changed)
    }

    fn materialize(&mut self, host_os: HostOs, default_location: &Path) {
        if !self.configured {
            self.entries = Self::defaults(host_os, default_location);
            self.configured = true;
        }
    }

    fn defaults(host_os: HostOs, location: &Path) -> Vec<FrontendSpec> {
        let host = |filesystem| FrontendSpec {
            filesystem,
            environment: Environment::Host,
            location: Some(location.to_path_buf()),
        };
        match host_os {
            HostOs::Linux => vec![host(Filesystem::Fuse)],
            HostOs::MacOs => vec![
                host(Filesystem::Nfs),
                FrontendSpec {
                    filesystem: Filesystem::Fuse,
                    environment: Environment::Docker,
                    location: None,
                },
            ],
            HostOs::Other => vec![host(Filesystem::Nfs)],
        }
    }
}

#[derive(Debug)]
pub struct ConfigDocument {
    path: PathBuf,
    document: DocumentMut,
    config: Config,
}

impl ConfigDocument {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let path = path.into();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(ConfigError::Io {
                    path,
                    source: error,
                });
            },
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| ConfigError::Parse {
            path: path.clone(),
            message: error.to_string(),
        })?;
        let document = text
            .parse::<DocumentMut>()
            .map_err(|error| ConfigError::Parse {
                path: path.clone(),
                message: error.to_string(),
            })?;
        reject_legacy_keys(&document)?;
        let config = deserialize(&document, &path)?;
        Ok(Self {
            path,
            document,
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn replace_frontends(&mut self, plan: &FrontendPlan) -> Result<(), ConfigError> {
        let mut plan = plan.clone();
        plan.configured = true;
        let item = plan.toml_item();
        let mut candidate = self.document.clone();
        candidate["frontends"] = item;
        reject_legacy_keys(&candidate)?;
        let config = deserialize(&candidate, &self.path)?;
        self.document = candidate;
        self.config = config;
        Ok(())
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let bytes = self.document.to_string().into_bytes();
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        write_atomic(&self.path, &bytes, 0o600).map_err(|source| ConfigError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Ok(ConfigDocument::load(path.as_ref().to_path_buf())?.config)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config at {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid frontend configuration: {0}")]
    Validation(String),
    #[error(
        "unsupported legacy frontend keys: {0}; replace them with filesystem, environment, and location (for example, [[frontends]] filesystem = \"fuse\" environment = \"host\" location = \"/mnt/omnifs\")"
    )]
    LegacyKeys(String),
}

fn deserialize<T: DeserializeOwned>(document: &DocumentMut, path: &Path) -> Result<T, ConfigError> {
    toml_edit::de::from_document(document.clone()).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

impl FrontendPlan {
    fn toml_item(&self) -> Item {
        if self.entries.is_empty() {
            return value(Array::new());
        }
        let mut tables = ArrayOfTables::new();
        for spec in &self.entries {
            let mut table = Table::new();
            table.insert("filesystem", value(spec.filesystem.label()));
            table.insert("environment", value(spec.environment.label()));
            if let Some(location) = &spec.location {
                table.insert("location", value(location.to_string_lossy().as_ref()));
            }
            tables.push(table);
        }
        Item::ArrayOfTables(tables)
    }
}

fn reject_legacy_keys(document: &DocumentMut) -> Result<(), ConfigError> {
    let mut keys = Vec::new();
    if let Some(Item::ArrayOfTables(tables)) = document.get("frontends") {
        for table in tables {
            collect_legacy_keys(table, &mut keys);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    if keys.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::LegacyKeys(keys.join(", ")))
    }
}

fn collect_legacy_keys(table: &Table, keys: &mut Vec<&'static str>) {
    for (key, _child) in table {
        match key {
            "kind" => keys.push("kind → filesystem"),
            "driver" => keys.push("driver → environment"),
            "mount_point" => keys.push("mount_point → location"),
            _ => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(
        filesystem: Filesystem,
        environment: Environment,
        location: Option<&str>,
    ) -> FrontendSpec {
        FrontendSpec {
            filesystem,
            environment,
            location: location.map(PathBuf::from),
        }
    }

    #[test]
    fn defaults_and_explicit_plan_replacement() {
        let location = "/home/user/omnifs";
        for (os, count) in [(HostOs::Linux, 1), (HostOs::MacOs, 2), (HostOs::Other, 1)] {
            assert_eq!(
                FrontendPlan::default()
                    .effective(os, location)
                    .unwrap()
                    .len(),
                count
            );
        }
        let mut plan = FrontendPlan::default();
        plan.enable(
            spec(Filesystem::Fuse, Environment::Krunkit, None),
            HostOs::MacOs,
            location,
        )
        .unwrap();
        let effective = plan.effective(HostOs::MacOs, location).unwrap();
        assert_eq!(effective.len(), 3);
        assert_eq!(effective[0].source, PlanSource::Configured);
    }

    #[test]
    fn validation_catches_compatibility_and_duplicates() {
        assert!(
            spec(Filesystem::Fuse, Environment::Host, None)
                .validate(HostOs::MacOs)
                .is_err()
        );
        assert!(
            spec(Filesystem::Nfs, Environment::Docker, None)
                .validate(HostOs::Linux)
                .is_err()
        );
        assert!(
            spec(Filesystem::Fuse, Environment::Docker, Some("/guest"))
                .validate(HostOs::Linux)
                .is_err()
        );
        let plan = FrontendPlan {
            entries: vec![
                spec(Filesystem::Nfs, Environment::Host, None),
                spec(
                    Filesystem::Fuse,
                    Environment::Host,
                    Some("/home/user/omnifs"),
                ),
            ],
            configured: true,
        };
        assert!(plan.effective(HostOs::Linux, "/home/user/omnifs").is_err());

        for host in [HostOs::Linux, HostOs::MacOs, HostOs::Other] {
            for filesystem in [Filesystem::Fuse, Filesystem::Nfs] {
                assert!(
                    spec(filesystem, Environment::Host, None)
                        .validate(host)
                        .is_ok()
                        == (filesystem == Filesystem::Nfs || host == HostOs::Linux)
                );
                assert!(
                    spec(filesystem, Environment::Docker, None)
                        .validate(host)
                        .is_ok()
                        == (filesystem == Filesystem::Fuse)
                );
                assert!(
                    spec(filesystem, Environment::Krunkit, None)
                        .validate(host)
                        .is_ok()
                        == (filesystem == Filesystem::Fuse)
                );
            }
        }
        let docker = FrontendSpec {
            filesystem: Filesystem::Fuse,
            environment: Environment::Docker,
            location: None,
        };
        let krunkit = FrontendSpec {
            filesystem: Filesystem::Fuse,
            environment: Environment::Krunkit,
            location: None,
        };
        let duplicates = FrontendPlan {
            entries: vec![docker.clone(), docker],
            configured: true,
        };
        assert!(
            duplicates
                .effective(HostOs::MacOs, "/home/user/omnifs")
                .is_err()
        );
        let duplicates = FrontendPlan {
            entries: vec![krunkit.clone(), krunkit],
            configured: true,
        };
        assert!(
            duplicates
                .effective(HostOs::MacOs, "/home/user/omnifs")
                .is_err()
        );
    }

    #[test]
    fn mutation_materializes_defaults_and_is_idempotent() {
        let location = "/home/user/omnifs";
        let mut linux = FrontendPlan::default();
        let fuse = spec(Filesystem::Fuse, Environment::Host, None);
        assert!(!linux.enable(fuse, HostOs::Linux, location).unwrap());
        assert_eq!(linux.entries.len(), 1);
        let id = linux.effective(HostOs::Linux, location).unwrap()[0].id();
        assert!(linux.disable(&id, HostOs::Linux, location).unwrap());
        assert!(linux.entries.is_empty());

        let mut mac = FrontendPlan::default();
        let nfs = spec(Filesystem::Nfs, Environment::Host, None);
        assert!(!mac.enable(nfs, HostOs::MacOs, location).unwrap());
        assert_eq!(mac.entries.len(), 2);
        let docker_id = mac.effective(HostOs::MacOs, location).unwrap()[1].id();
        assert!(mac.disable(&docker_id, HostOs::MacOs, location).unwrap());
        assert_eq!(mac.entries.len(), 1);

        let snapshot = mac.clone();
        let invalid = spec(Filesystem::Nfs, Environment::Docker, None);
        assert!(mac.enable(invalid, HostOs::MacOs, location).is_err());
        assert_eq!(mac, snapshot);
    }

    #[test]
    fn document_preserves_comments_and_writes_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# keep me\n[system]\nfrontend_image = \"x\"\n\n[[frontends]]\nfilesystem = \"nfs\"\nenvironment = \"host\"\nlocation = \"/old\"\n").unwrap();
        let mut document = ConfigDocument::load(&path).unwrap();
        let mut plan = FrontendPlan::default();
        plan.enable(
            spec(Filesystem::Fuse, Environment::Krunkit, None),
            HostOs::MacOs,
            "/old",
        )
        .unwrap();
        document.replace_frontends(&plan).unwrap();
        document.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert!(text.contains("frontend_image = \"x\""));
        assert!(text.contains("filesystem = \"fuse\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let original = text;
        document.path = dir.path().to_path_buf();
        assert!(document.save().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn old_keys_are_targeted_and_unknown_keys_are_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[[frontends]]\nkind = \"fuse\"\ndriver = \"local\"\nmount_point = \"/tmp\"\n",
        )
        .unwrap();
        let error = ConfigDocument::load(&path).unwrap_err().to_string();
        assert!(error.contains("kind"));
        assert!(error.contains("filesystem"));
        assert!(error.contains("environment"));
        assert!(error.contains("location"));
        std::fs::write(&path, "[telemetry]\nenabled = true\nwat = 1\n").unwrap();
        assert!(ConfigDocument::load(&path).is_err());

        // Legacy-looking keys in unrelated future tables are ordinary
        // unknown fields and are not reported as frontend migrations.
        std::fs::write(&path, "[future]\nkind = \"opaque\"\n").unwrap();
        let error = ConfigDocument::load(&path).unwrap_err().to_string();
        assert!(!error.contains("legacy"));
    }

    #[test]
    fn valid_frontend_array_parses_with_new_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[[frontends]]\nfilesystem = \"nfs\"\nenvironment = \"host\"\nlocation = \"/mnt\"\n\n[[frontends]]\nfilesystem = \"fuse\"\nenvironment = \"docker\"\n",
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.frontends.entries.len(), 2);
        assert_eq!(config.frontends.entries[0].filesystem, Filesystem::Nfs);
        assert_eq!(config.frontends.entries[1].environment, Environment::Docker);
    }

    #[test]
    fn explicit_empty_plan_survives_disable_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut document = ConfigDocument::load(&path).unwrap();
        let id = FrontendPlan::default()
            .effective(HostOs::Linux, "/home/user/omnifs")
            .unwrap()[0]
            .id();
        let mut plan = document.config.frontends.clone();
        assert!(
            plan.disable(&id, HostOs::Linux, "/home/user/omnifs")
                .unwrap()
        );
        assert!(
            plan.effective(HostOs::Linux, "/home/user/omnifs")
                .unwrap()
                .is_empty()
        );
        document.replace_frontends(&plan).unwrap();
        document.save().unwrap();
        let loaded = ConfigDocument::load(&path).unwrap();
        assert!(
            loaded
                .config
                .frontends
                .effective(HostOs::Linux, "/home/user/omnifs")
                .unwrap()
                .is_empty()
        );
    }
}
