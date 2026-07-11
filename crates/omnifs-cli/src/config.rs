//! Global `config.toml` loader. Lives at `paths.config_file`.
//!
//! Resolution order is: CLI flag > env var > config file > built-in default.
//! Missing file is not an error; malformed file is. Commands load it from
//! their resolved workspace when they need the optional frontend's image.

use anyhow::{Context, Result, ensure};
use omnifs_workspace::runtime_record::FrontendKind;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::frontend_backend::Driver;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub system: ConfigSystem,
    pub telemetry: ConfigTelemetry,
    pub frontend: ConfigFrontend,
    /// Declarative frontend launch plan, parsed from `[[frontends]]` entries.
    /// Absent or empty means "use the platform default plan"; see
    /// [`resolve_frontends`]. An explicit non-empty list replaces the
    /// platform default entirely rather than merging with it.
    pub frontends: Vec<ConfigFrontendEntry>,
}

/// Local-only dogfood telemetry policy. On by default; `[telemetry] enabled =
/// false` opts out. The CLI honors it for its own `cli.jsonl` writer and
/// propagates it to the daemon it launches (via `OMNIFS_TELEMETRY`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigTelemetry {
    pub enabled: bool,
}

impl Default for ConfigTelemetry {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSystem {
    /// Override for the optional Docker-hosted FUSE frontend's image. The
    /// daemon itself always runs host-native, so there is no daemon runtime
    /// mode to configure here; this is an opt-in attachment
    /// (`omnifs frontend up`), not a daemon launch policy.
    pub frontend_image: Option<String>,
}

/// Frontend delivery settings shared across every launched frontend. The
/// per-frontend `kind`/`driver`/`mount_point` choice itself lives in
/// [`ConfigFrontendEntry`] (`[[frontends]]`), not here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFrontend {
    /// Override for the krunkit driver's guest disk image. A dev binary
    /// defaults to the local `target/guest-image/omnifs-guest.raw` (see
    /// `just guest-image`) and never downloads; a release binary defaults to
    /// the pinned ghcr OCI artifact tag and pulls it on first use (see
    /// `crate::guest_image_pull`). Irrelevant to the Docker driver.
    pub guest_image: Option<String>,
}

/// One `[[frontends]]` entry: a frontend `omnifs up` launches, or a candidate
/// `omnifs frontend up --driver` can select. `kind` is the frontend protocol;
/// `driver` is how the CLI delivers that protocol's runner process.
/// `mount_point` only applies to `driver = "local"`, since Docker and
/// krunkit own their mount inside the guest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFrontendEntry {
    pub kind: FrontendKind,
    pub driver: Driver,
    pub mount_point: Option<PathBuf>,
}

/// Host OS as it matters for frontend driver resolution: local FUSE is
/// Linux-only host-side, and the macOS platform default additionally
/// attaches the Docker-hosted FUSE frontend. A testable indirection over
/// `cfg!(target_os = ...)` so [`resolve_frontends`] can be table-tested
/// across every platform branch from one test binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOs {
    Linux,
    MacOs,
    Other,
}

impl HostOs {
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Other
        }
    }
}

/// Whether a resolved frontend came from an explicit `[[frontends]]` entry or
/// from the platform default plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provenance {
    Explicit,
    Default,
}

/// One frontend `omnifs up` or `omnifs frontend up` launches, after
/// resolving `[[frontends]]` config against the host platform and the
/// default mount point.
#[derive(Debug, Clone)]
pub(crate) struct EffectiveFrontend {
    pub(crate) kind: FrontendKind,
    pub(crate) driver: Driver,
    /// Resolved absolute mount point; `Some` for local entries, `None` for
    /// docker/krunkit entries (the mount lives inside the guest).
    pub(crate) mount_point: Option<PathBuf>,
    pub(crate) provenance: Provenance,
}

/// Turn parsed `[[frontends]]` entries into the effective launch plan.
///
/// Validation runs in this order: driver/kind compatibility and the
/// local-mount-point restriction on docker/krunkit entries, then the
/// local-FUSE host restriction, then per-driver cardinality, then duplicate
/// detection. Local entries resolve their mount point (explicit
/// `mount_point`, else `default_mount_point`) before duplicates are
/// detected, so an entry that omits `mount_point` and one that names the
/// default explicitly collide.
///
/// An explicit non-empty list replaces the platform default plan entirely;
/// there is no merging. The default plan is: Linux → one local FUSE
/// frontend; macOS → one local NFS frontend plus the Docker-hosted FUSE
/// frontend; any other host → one local NFS frontend.
pub(crate) fn resolve_frontends(
    entries: &[ConfigFrontendEntry],
    host_os: HostOs,
    default_mount_point: &Path,
) -> Result<Vec<EffectiveFrontend>> {
    if entries.is_empty() {
        return Ok(default_frontends(host_os, default_mount_point));
    }

    let mut resolved = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry.driver {
            Driver::Docker | Driver::Krunkit => {
                ensure!(
                    entry.kind == FrontendKind::Fuse,
                    "the {} driver only delivers a fuse frontend, got kind = \"{}\"",
                    entry.driver.as_via().label(),
                    entry.kind.label()
                );
                ensure!(
                    entry.mount_point.is_none(),
                    "the {} driver owns its mount inside the guest; mount_point is not allowed",
                    entry.driver.as_via().label()
                );
            },
            Driver::Local => {
                ensure!(
                    entry.kind != FrontendKind::Fuse || host_os == HostOs::Linux,
                    "a local fuse frontend requires a Linux host"
                );
            },
        }
        let mount_point = match entry.driver {
            Driver::Local => Some(
                entry
                    .mount_point
                    .clone()
                    .unwrap_or_else(|| default_mount_point.to_path_buf()),
            ),
            Driver::Docker | Driver::Krunkit => None,
        };
        resolved.push(EffectiveFrontend {
            kind: entry.kind,
            driver: entry.driver,
            mount_point,
            provenance: Provenance::Explicit,
        });
    }

    let mut docker_seen = false;
    let mut krunkit_seen = false;
    let mut local_seen: Vec<PathBuf> = Vec::new();
    for effective in &resolved {
        match effective.driver {
            Driver::Docker => {
                ensure!(!docker_seen, "at most one docker frontend entry is allowed");
                docker_seen = true;
            },
            Driver::Krunkit => {
                ensure!(
                    !krunkit_seen,
                    "at most one krunkit frontend entry is allowed"
                );
                krunkit_seen = true;
            },
            Driver::Local => {
                let mount_point = effective
                    .mount_point
                    .clone()
                    .expect("local entries always resolve a mount point");
                // One mount point serves one frontend, whatever the kind: two
                // entries of different kinds at the same resolved path would
                // race for the same mount, so the collision check ignores kind.
                ensure!(
                    !local_seen.contains(&mount_point),
                    "two local frontend entries resolve to the same mount point {}",
                    mount_point.display()
                );
                local_seen.push(mount_point);
            },
        }
    }

    Ok(resolved)
}

/// The platform default plan when `[[frontends]]` is absent or empty.
fn default_frontends(host_os: HostOs, default_mount_point: &Path) -> Vec<EffectiveFrontend> {
    let local = |kind: FrontendKind| EffectiveFrontend {
        kind,
        driver: Driver::Local,
        mount_point: Some(default_mount_point.to_path_buf()),
        provenance: Provenance::Default,
    };
    match host_os {
        HostOs::Linux => vec![local(FrontendKind::Fuse)],
        HostOs::MacOs => vec![
            local(FrontendKind::Nfs),
            EffectiveFrontend {
                kind: FrontendKind::Fuse,
                driver: Driver::Docker,
                mount_point: None,
                provenance: Provenance::Default,
            },
        ],
        HostOs::Other => vec![local(FrontendKind::Nfs)],
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            },
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", path.display()));
            },
        };
        toml::from_str(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    /// Effective telemetry state for this process: the persistent
    /// `[telemetry] enabled` config field combined with the `OMNIFS_TELEMETRY`
    /// env kill switch, so either one can turn it off.
    pub fn telemetry_enabled(&self) -> bool {
        self.telemetry.enabled && omnifs_workspace::telemetry::enabled_from_env()
    }
}

/// Resolve one setting through the single CLI precedence chain:
/// CLI flag > env var > config file > built-in default.
///
/// The env var is read through [`env_string`] (an empty value
/// counts as unset) and parsed into `T`; an unset, empty, or unparseable value
/// falls through to the config source and finally the default. Every CLI
/// setting resolves through this one chain so precedence lives in a single
/// place. `from_config` is a thunk rather than a `Fn(&Config)` so callers with
/// no config source (e.g. the daemon control address) can pass `|| None`.
pub(crate) fn resolve_setting<T: FromStr>(
    flag: Option<T>,
    env: &str,
    from_config: impl FnOnce() -> Option<T>,
    default: T,
) -> T {
    flag.or_else(|| env_string(env).and_then(|value| value.parse().ok()))
        .or_else(from_config)
        .unwrap_or(default)
}

pub(crate) fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_defaults_on_and_parses_off_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        // Absent config: telemetry defaults on.
        let default = Config::load(&path).unwrap();
        assert!(default.telemetry.enabled);

        // Explicit off-switch parses and disables.
        std::fs::write(&path, "[telemetry]\nenabled = false\n").unwrap();
        let off = Config::load(&path).unwrap();
        assert!(!off.telemetry.enabled);

        // A typo'd key is rejected by the strict parser.
        std::fs::write(&path, "[telemetry]\nenabbled = false\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    fn entry(kind: FrontendKind, driver: Driver, mount_point: Option<&str>) -> ConfigFrontendEntry {
        ConfigFrontendEntry {
            kind,
            driver,
            mount_point: mount_point.map(PathBuf::from),
        }
    }

    fn default_mount() -> PathBuf {
        PathBuf::from("/home/user/omnifs")
    }

    #[test]
    fn os_defaults_apply_when_frontends_is_absent_or_empty() {
        let cases: &[(HostOs, Vec<EffectiveFrontend>)] = &[
            (
                HostOs::Linux,
                vec![EffectiveFrontend {
                    kind: FrontendKind::Fuse,
                    driver: Driver::Local,
                    mount_point: Some(default_mount()),
                    provenance: Provenance::Default,
                }],
            ),
            (
                HostOs::MacOs,
                vec![
                    EffectiveFrontend {
                        kind: FrontendKind::Nfs,
                        driver: Driver::Local,
                        mount_point: Some(default_mount()),
                        provenance: Provenance::Default,
                    },
                    EffectiveFrontend {
                        kind: FrontendKind::Fuse,
                        driver: Driver::Docker,
                        mount_point: None,
                        provenance: Provenance::Default,
                    },
                ],
            ),
            (
                HostOs::Other,
                vec![EffectiveFrontend {
                    kind: FrontendKind::Nfs,
                    driver: Driver::Local,
                    mount_point: Some(default_mount()),
                    provenance: Provenance::Default,
                }],
            ),
        ];

        for (host_os, expected) in cases {
            // Resolver-level, "absent" and "empty" are the same input: an
            // empty entries slice. The TOML-parsing distinction (no
            // `[[frontends]]` table at all vs. `frontends = []`) is covered
            // by `frontends_array_round_trips_and_singular_driver_is_retired`.
            let resolved = resolve_frontends(&[], *host_os, &default_mount()).unwrap();
            assert_eq!(resolved.len(), expected.len(), "{host_os:?}");
            for (actual, want) in resolved.iter().zip(expected) {
                assert_eq!(actual.kind, want.kind);
                assert_eq!(actual.driver, want.driver);
                assert_eq!(actual.mount_point, want.mount_point);
                assert_eq!(actual.provenance, want.provenance);
            }
        }
    }

    #[test]
    fn explicit_nonempty_list_replaces_defaults_entirely() {
        // macOS with only a krunkit entry produces exactly that one entry,
        // not the local-NFS default plus krunkit.
        let entries = vec![entry(FrontendKind::Fuse, Driver::Krunkit, None)];
        let resolved = resolve_frontends(&entries, HostOs::MacOs, &default_mount()).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].driver, Driver::Krunkit);
        assert_eq!(resolved[0].kind, FrontendKind::Fuse);
        assert_eq!(resolved[0].mount_point, None);
        assert_eq!(resolved[0].provenance, Provenance::Explicit);
    }

    #[test]
    fn mount_point_precedence_explicit_wins_omitted_falls_back() {
        let entries = vec![
            entry(FrontendKind::Nfs, Driver::Local, Some("/mnt/explicit")),
            entry(FrontendKind::Fuse, Driver::Local, None),
        ];
        let resolved = resolve_frontends(&entries, HostOs::Linux, &default_mount()).unwrap();
        assert_eq!(
            resolved[0].mount_point,
            Some(PathBuf::from("/mnt/explicit"))
        );
        assert_eq!(resolved[1].mount_point, Some(default_mount()));
    }

    #[test]
    fn rejects_docker_or_krunkit_with_non_fuse_kind() {
        for driver in [Driver::Docker, Driver::Krunkit] {
            let entries = vec![entry(FrontendKind::Nfs, driver, None)];
            assert!(resolve_frontends(&entries, HostOs::MacOs, &default_mount()).is_err());
        }
    }

    #[test]
    fn rejects_docker_or_krunkit_with_mount_point() {
        for driver in [Driver::Docker, Driver::Krunkit] {
            let entries = vec![entry(FrontendKind::Fuse, driver, Some("/mnt/guest"))];
            assert!(resolve_frontends(&entries, HostOs::MacOs, &default_mount()).is_err());
        }
    }

    #[test]
    fn rejects_local_fuse_off_linux() {
        let entries = vec![entry(FrontendKind::Fuse, Driver::Local, None)];
        assert!(resolve_frontends(&entries, HostOs::MacOs, &default_mount()).is_err());
        assert!(resolve_frontends(&entries, HostOs::Other, &default_mount()).is_err());
        // Linux is fine.
        assert!(resolve_frontends(&entries, HostOs::Linux, &default_mount()).is_ok());
    }

    #[test]
    fn rejects_more_than_one_docker_or_krunkit_entry() {
        let two_docker = vec![
            entry(FrontendKind::Fuse, Driver::Docker, None),
            entry(FrontendKind::Fuse, Driver::Docker, None),
        ];
        assert!(resolve_frontends(&two_docker, HostOs::MacOs, &default_mount()).is_err());

        let two_krunkit = vec![
            entry(FrontendKind::Fuse, Driver::Krunkit, None),
            entry(FrontendKind::Fuse, Driver::Krunkit, None),
        ];
        assert!(resolve_frontends(&two_krunkit, HostOs::MacOs, &default_mount()).is_err());
    }

    #[test]
    fn rejects_duplicate_local_entries_after_mount_point_resolution() {
        // One entry omits `mount_point` (falls back to the default), the
        // other names that same default explicitly: same resolved identity.
        let entries = vec![
            entry(FrontendKind::Nfs, Driver::Local, None),
            entry(
                FrontendKind::Nfs,
                Driver::Local,
                Some(default_mount().to_str().unwrap()),
            ),
        ];
        assert!(resolve_frontends(&entries, HostOs::MacOs, &default_mount()).is_err());
    }

    #[test]
    fn rejects_cross_kind_local_entries_at_the_same_mount_point() {
        // Different kinds do not make the entries compatible: one mount point
        // serves one frontend, so a fuse and an nfs entry resolving to the
        // same path must collide (Linux, the one host where both kinds are
        // locally valid).
        let entries = vec![
            entry(FrontendKind::Fuse, Driver::Local, None),
            entry(
                FrontendKind::Nfs,
                Driver::Local,
                Some(default_mount().to_str().unwrap()),
            ),
        ];
        let error = resolve_frontends(&entries, HostOs::Linux, &default_mount()).unwrap_err();
        assert!(error.to_string().contains("same mount point"));
    }

    #[test]
    fn frontends_array_round_trips_and_singular_driver_is_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        // No config file at all: `frontends` defaults to empty.
        assert!(Config::load(&path).unwrap().frontends.is_empty());

        // An explicit empty inline array parses to empty too.
        std::fs::write(&path, "frontends = []\n").unwrap();
        assert!(Config::load(&path).unwrap().frontends.is_empty());

        std::fs::write(
            &path,
            r#"
[[frontends]]
kind = "nfs"
driver = "local"
mount_point = "/Users/me/omnifs"

[[frontends]]
kind = "fuse"
driver = "docker"
"#,
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.frontends.len(), 2);
        assert_eq!(config.frontends[0].kind, FrontendKind::Nfs);
        assert_eq!(config.frontends[0].driver, Driver::Local);
        assert_eq!(
            config.frontends[0].mount_point,
            Some(PathBuf::from("/Users/me/omnifs"))
        );
        assert_eq!(config.frontends[1].kind, FrontendKind::Fuse);
        assert_eq!(config.frontends[1].driver, Driver::Docker);
        assert_eq!(config.frontends[1].mount_point, None);

        // An entry with an unknown field is rejected by the strict parser.
        std::fs::write(
            &path,
            "[[frontends]]\nkind = \"fuse\"\ndriver = \"docker\"\nbogus = 1\n",
        )
        .unwrap();
        assert!(Config::load(&path).is_err());

        // The retired singular `[frontend] driver` field no longer parses.
        std::fs::write(&path, "[frontend]\ndriver = \"docker\"\n").unwrap();
        assert!(Config::load(&path).is_err());

        // `[frontend] guest_image` still parses.
        std::fs::write(&path, "[frontend]\nguest_image = \"custom:tag\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.frontend.guest_image.as_deref(), Some("custom:tag"));
    }
}
