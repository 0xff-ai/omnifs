//! `omnifs shell` — drop into a subshell tuned for exploring the projected tree.
//!
//! Launches the user's `$SHELL` as a child process pointed at an omnifs-owned rc
//! (zsh via `ZDOTDIR`, bash via `--rcfile`). That rc inherits the user's own
//! config, then takes over the prompt with one computed only from `$PWD` and a
//! mount→provider map handed in at launch. The point is to stop the user's
//! prompt framework (starship, powerlevel10k, …) from re-scanning the cwd on
//! every render: that scan is a filesystem probe, and on a lazy projection each
//! probe is a provider round-trip. Because it is a child process, `exit` returns
//! the user exactly where they were with nothing to undo; their real dotfiles
//! are never touched.
//!
//! Surface-aware: several frontends can be attached at once (`[[frontends]]`
//! config), so `omnifs shell` probes live state and picks one rather than
//! trusting a single recorded choice. A guest frontend's mount (Docker
//! container or krunkit microVM) is invisible on the host, so a live guest is
//! preferred over a host subshell and entered by execing into it (`docker
//! exec` or ssh-over-vsock); Docker is checked before krunkit only because
//! both are vanishingly unlikely to run at once and a deterministic order
//! beats an arbitrary one. With no guest running, a live local mount is used
//! instead; more than one live local mount is selected with a TTY picker or
//! reported as an ambiguity in headless mode. `--mount` accepts a unique local
//! mount-point basename or exact path and always bypasses guest preference.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use omnifs_api::{DaemonStatus, FrontendDelivery, MountInfo};
use omnifs_mtab::MountState;

use crate::frontend_backend::{DockerBackend, FrontendBackend};
use crate::frontend_container::FRONTEND_DEV_IMAGE;
use crate::krunkit_backend::{self, KrunkitBackend};
use crate::launch_backend::{ContainerName, DockerTarget, GUEST_MOUNT};
use crate::runtime::Runtime;
use crate::workspace::Workspace;
use omnifs_workspace::layout::{OMNIFS_MOUNT_POINT_ENV, WorkspaceLayout};

/// Shell should remain responsive when the daemon is down or wedged. The
/// control client has a five-second request timeout for ordinary commands,
/// which is too long for shell's best-effort status and attachment lookup.
/// Runner-owned records provide the offline fallback for local mounts.
const SHELL_STATUS_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Args, Debug, Clone, Default)]
pub struct ShellArgs {
    /// Start a clean shell that does not source your shell rc files.
    #[arg(long)]
    pub hermetic: bool,
    /// Shell to launch (defaults to `$SHELL`).
    #[arg(long)]
    pub shell: Option<String>,
    /// Select a local frontend mount by its final path component or exact path.
    /// This chooses the host mount root, not a provider mount beneath it, and
    /// bypasses guest frontend preference.
    #[arg(long, value_name = "NAME")]
    pub mount: Option<String>,
    /// Run a command in the mount context instead of an interactive shell.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl ShellArgs {
    pub async fn run(self) -> Result<()> {
        if std::env::var_os("OMNIFS_IN_SHELL").is_some() {
            crate::ui::narrate(
                "note: already inside an omnifs shell; opening a nested one (exit twice to return)",
            );
        }

        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();

        // Probe guest state before asking the daemon for optional banner facts.
        // A one-shot guest command does not need status at all, so a dead
        // daemon cannot add its five-second control timeout to the useful path.
        // Local target discovery still uses daemon attachments when status
        // answers, and runner-owned state when it does not.
        let guest = if self.mount.is_none() {
            if docker_frontend_is_running(paths).await {
                let container_name = crate::frontend_container::frontend_container_name(paths)?;
                Some(ShellTarget::Docker(container_name))
            } else if krunkit_frontend_is_running(paths).await {
                Some(ShellTarget::Krunkit)
            } else {
                None
            }
        } else {
            None
        };

        let status = if guest.is_some() && !self.command.is_empty() {
            None
        } else {
            shell_status(&workspace).await
        };
        let configured_roots = if self.command.is_empty() && status.is_none() {
            match workspace.mounts() {
                Ok(mounts) => mounts
                    .into_iter()
                    .map(|mount| mount.name.to_string())
                    .collect(),
                Err(error) => {
                    crate::ui::narrate(format!(
                        "note: could not read configured mounts for shell banner: {error:#}"
                    ));
                    Vec::new()
                },
            }
        } else {
            Vec::new()
        };
        let context = ShellContext::new(status, configured_roots);

        // The offline path is still useful, but tell the user before selection
        // can fail so a missing/stale mount does not hide the actionable daemon
        // recovery hint.
        if context.status.is_none() && guest.is_none() {
            crate::ui::narrate(
                "note: the daemon is not answering; its mount may be stale (try `omnifs up`)",
            );
        }

        // When a guest target is live and no selector was requested, avoid
        // probing host mount records at all. Otherwise resolve the local
        // frontend from daemon attachments or runner-owned state.
        let local = if self.mount.is_some() || guest.is_none() {
            Some(LocalMounts::discover(paths, context.status.as_ref())?)
        } else {
            None
        };

        let target = context.choose_target(
            local,
            self.mount.as_deref(),
            crate::ui::prompt::is_terminal(),
            guest,
        )?;

        let mounts = context
            .status
            .as_ref()
            .map(|status| status.mounts.clone())
            .unwrap_or_default();

        // A one-shot command runs directly in the mount context; no rc or prompt
        // tuning is needed for a non-interactive invocation.
        if !self.command.is_empty() {
            return match target {
                ShellTarget::Docker(container_name) => self.exec_in_container(&container_name),
                ShellTarget::Krunkit => self.exec_in_krunkit_guest(paths),
                ShellTarget::Local(mount_point) => {
                    let mut cmd = Command::new(&self.command[0]);
                    cmd.args(&self.command[1..]);
                    apply_context_env(&mut cmd, &mount_point, &mounts, self.hermetic);
                    set_cwd_to_mount(&mut cmd, &mount_point);
                    spawn_and_propagate(cmd, format!("run `{}`", self.command[0]))
                },
            };
        }

        match target {
            ShellTarget::Docker(container_name) => {
                self.exec_in_container_with_banner(&container_name, &context)
            },
            ShellTarget::Krunkit => self.exec_in_krunkit_guest_with_banner(paths, &context),
            ShellTarget::Local(mount_point) => {
                self.exec_local_shell(&mount_point, &mounts, paths, &context)
            },
        }
    }

    /// Attach to the optional FUSE frontend by execing into it, landing in
    /// the projected tree. The minimal frontend image ships only `/bin/sh`,
    /// so no host rc plumbing applies here; `--shell` overrides the default
    /// and a trailing command runs non-interactively.
    ///
    /// Uses [`DockerBackend`] only for command construction; the image field of
    /// its `DockerTarget`
    /// is unused here, so the dev placeholder is fine regardless of build
    /// channel, mirroring `frontend down`.
    fn exec_in_container(&self, container_name: &ContainerName) -> Result<()> {
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            FRONTEND_DEV_IMAGE.to_string(),
        )?;
        let backend = DockerBackend::new(Runtime::connect_for(&target)?);
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, format!("open shell in container `{container_name}`"))
    }

    fn exec_in_container_with_banner(
        &self,
        container_name: &ContainerName,
        context: &ShellContext,
    ) -> Result<()> {
        crate::ui::narrate(context.banner("container", Path::new(GUEST_MOUNT)));
        self.exec_in_container(container_name)
    }

    /// Attach to the krunkit guest over ssh-over-vsock, landing in the
    /// projected tree. `shell_command` is pure construction (no I/O), so the
    /// `socat` probe (an I/O check) happens here, at the one call site about
    /// to actually run it.
    fn exec_in_krunkit_guest(&self, paths: &WorkspaceLayout) -> Result<()> {
        krunkit_backend::ensure_socat_available()?;
        let backend = KrunkitBackend::new(paths.config_dir.clone());
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        spawn_and_propagate(cmd, "open shell in the krunkit guest".to_string())
    }

    fn exec_in_krunkit_guest_with_banner(
        &self,
        paths: &WorkspaceLayout,
        context: &ShellContext,
    ) -> Result<()> {
        crate::ui::narrate(context.banner("krunkit", Path::new(GUEST_MOUNT)));
        self.exec_in_krunkit_guest(paths)
    }

    fn exec_local_shell(
        &self,
        mount_point: &Path,
        mounts: &[MountInfo],
        paths: &WorkspaceLayout,
        context: &ShellContext,
    ) -> Result<()> {
        let shell = resolve_shell(self.shell.as_deref());
        let shell_dir = paths.cache_dir.join("shell");
        let mut cmd = Command::new(&shell);
        match ShellKind::detect(&shell) {
            ShellKind::Zsh => {
                let zdotdir = shell_dir.join("zsh");
                std::fs::create_dir_all(&zdotdir)
                    .with_context(|| format!("create {}", zdotdir.display()))?;
                std::fs::write(zdotdir.join(".zshenv"), ZSH_ZSHENV)?;
                std::fs::write(zdotdir.join(".zshrc"), ZSH_ZSHRC)?;
                cmd.arg("-i");
                cmd.env("ZDOTDIR", &zdotdir);
                cmd.env("OMNIFS_PREV_ZDOTDIR", prev_zdotdir());
            },
            ShellKind::Bash => {
                std::fs::create_dir_all(&shell_dir)
                    .with_context(|| format!("create {}", shell_dir.display()))?;
                let rcfile = shell_dir.join("omnifs.bashrc");
                std::fs::write(&rcfile, BASH_RC)?;
                cmd.arg("--rcfile").arg(&rcfile).arg("-i");
            },
            ShellKind::Other => {
                // No rc lever for this shell: give it the omnifs context and the
                // mount as cwd, but leave its prompt alone.
            },
        }
        apply_context_env(&mut cmd, mount_point, mounts, self.hermetic);
        set_cwd_to_mount(&mut cmd, mount_point);

        crate::ui::narrate(context.banner("local", mount_point));
        spawn_and_propagate(cmd, "launch omnifs shell".to_string())
    }
}

/// Whether the workspace's frontend container exists and is running. A
/// best-effort probe: any failure to even reach Docker is "not running"
/// rather than a hard error, since `omnifs shell` should still fall through
/// to krunkit or a local mount when Docker is simply unavailable. Mirrors the
/// discovery in `commands/frontend/status.rs`.
async fn docker_frontend_is_running(paths: &WorkspaceLayout) -> bool {
    let Ok(container_name) = crate::frontend_container::frontend_container_name(paths) else {
        return false;
    };
    let Ok(target) = DockerTarget::new(
        container_name.as_str().to_string(),
        FRONTEND_DEV_IMAGE.to_string(),
    ) else {
        return false;
    };
    let Ok(runtime) = Runtime::connect_for(&target) else {
        return false;
    };
    matches!(
        DockerBackend::new(runtime).is_running().await,
        Ok(Some(true))
    )
}

/// Whether the workspace's krunkit guest exists and is running. Same
/// best-effort probe policy as [`docker_frontend_is_running`].
async fn krunkit_frontend_is_running(paths: &WorkspaceLayout) -> bool {
    matches!(
        KrunkitBackend::new(paths.config_dir.clone())
            .is_running()
            .await,
        Ok(Some(true))
    )
}

/// Best-effort daemon facts for the shell banner and local attachment roster.
/// Target selection must not inherit the control client's five-second request
/// timeout when the daemon is unavailable, so bound this optional lookup to a
/// short UX budget and fall back to runner-owned state on expiry.
async fn shell_status(workspace: &Workspace) -> Option<DaemonStatus> {
    tokio::time::timeout(SHELL_STATUS_TIMEOUT, workspace.daemon().status())
        .await
        .ok()
        .and_then(Result::ok)
}

/// The target surface selected for one shell invocation. A local target is a
/// host-visible frontend mount point. A guest target is deliberately opaque to
/// the host because its mount lives in another namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellTarget {
    Local(PathBuf),
    Docker(ContainerName),
    Krunkit,
}

/// Runtime facts and the root-mount roster used by one shell invocation.
///
/// `DaemonStatus.mounts` is authoritative when available. Configured names
/// are used only when the daemon cannot answer and are labelled unverified in
/// the banner, so a stale config cannot be presented as a live root.
#[derive(Debug, Clone)]
struct ShellContext {
    status: Option<DaemonStatus>,
    configured_roots: Vec<String>,
}

impl ShellContext {
    fn new(status: Option<DaemonStatus>, configured_roots: Vec<String>) -> Self {
        Self {
            status,
            configured_roots,
        }
    }

    /// Sorted, deduplicated root names for the interactive banner.
    fn root_mounts(&self) -> Vec<String> {
        let names = self.status.as_ref().map_or_else(
            || self.configured_roots.clone(),
            |status| {
                status
                    .mounts
                    .iter()
                    .map(|mount| mount.mount.clone())
                    .collect()
            },
        );
        sorted_unique(names)
    }

    /// Resolve an invocation target. Explicit local selection has precedence
    /// over a guest target; this is what makes `--mount` a reliable escape hatch
    /// when Docker or krunkit is also running.
    fn choose_target(
        &self,
        local: Option<LocalMounts>,
        selector: Option<&str>,
        interactive: bool,
        guest: Option<ShellTarget>,
    ) -> Result<ShellTarget> {
        if selector.is_some() {
            let Some(local) = local else {
                if self.status.is_some() {
                    anyhow::bail!(
                        "no local frontend is attached for `--mount`; start it with `omnifs frontend up`"
                    );
                }
                anyhow::bail!("no host mount is available for `--mount`");
            };
            return local.select(selector, false);
        }
        if let Some(guest) = guest {
            return Ok(guest);
        }
        let Some(local) = local else {
            anyhow::bail!("no host mount is available");
        };
        local.select(None, interactive)
    }

    fn banner(&self, surface: &str, location: &Path) -> String {
        let roots = self.root_mounts();
        let (label, roster) = if self.status.is_some() {
            (
                "root mounts",
                if roots.is_empty() {
                    "(none)".to_string()
                } else {
                    roots.join(", ")
                },
            )
        } else if roots.is_empty() {
            ("root mounts (unavailable)", "(unknown)".to_string())
        } else {
            ("configured roots (unverified)", roots.join(", "))
        };
        format!(
            "omnifs shell ({surface}) at {} ({label}: {roster}; type `exit` to leave)",
            location.display()
        )
    }
}

/// One host-visible local frontend mount point. The final path component is
/// the short selector accepted by `--mount`; an exact path disambiguates two
/// local frontends with the same basename.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LocalMount(PathBuf);

impl LocalMount {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn has_name(&self, name: &str) -> bool {
        self.path()
            .file_name()
            .and_then(|component| component.to_str())
            == Some(name)
    }
}

impl fmt::Display for LocalMount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.path().display().fmt(f)
    }
}

/// The set of live local frontend mount points and their selection policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalMounts(Vec<LocalMount>);

impl LocalMounts {
    fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut paths = paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty())
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        Self(paths.into_iter().map(LocalMount::new).collect())
    }

    /// The daemon's attachment list is authoritative when it answers. When it
    /// does not, use runner-owned records because the daemon may be down while
    /// a local frontend is still mounted.
    fn discover(paths: &WorkspaceLayout, live: Option<&DaemonStatus>) -> Result<Self> {
        if let Some(status) = live {
            return Ok(Self::from_paths(
                status
                    .frontends
                    .iter()
                    .filter(|frontend| {
                        frontend.delivery == FrontendDelivery::Local
                            && !frontend.mount_point.as_os_str().is_empty()
                    })
                    .map(|frontend| frontend.mount_point.clone()),
            ));
        }

        let mut candidates = Vec::new();
        for path in MountState::files_under(&paths.frontend_state_root())
            .context("discover local frontend records")?
        {
            match MountState::read_file(&path) {
                Ok(state) => {
                    // A record whose mount is no longer live is teardown debris,
                    // not a shell destination. Without the daemon feature the
                    // ownership probe does not exist, so records are trusted.
                    #[cfg(feature = "daemon")]
                    if !crate::host_teardown::local_mount_is_owned(&state) {
                        continue;
                    }
                    candidates.push(state.mount_point);
                },
                Err(error) => {
                    crate::ui::eprint_raw(&format!(
                        "⚠  Skipping local frontend record {}: {error}\n",
                        path.display()
                    ));
                },
            }
        }
        Ok(Self::from_paths(candidates))
    }

    /// Select a local frontend by exact path or unique basename. With no
    /// selector, one candidate wins; multiple candidates use the interactive
    /// picker only when a terminal is available.
    fn select(&self, selector: Option<&str>, interactive: bool) -> Result<ShellTarget> {
        if let Some(selector) = selector {
            return self.select_named(selector);
        }
        match self.0.as_slice() {
            [] => anyhow::bail!("no host mount is available; start it with `omnifs frontend up`"),
            [mount] => Ok(ShellTarget::Local(mount.path().to_path_buf())),
            mounts if interactive => {
                let selected =
                    crate::ui::prompt::Select::new("Which local frontend mount should omnifs use?")
                        .items(mounts.iter().cloned())
                        .ask()?;
                Ok(ShellTarget::Local(selected.path().to_path_buf()))
            },
            mounts => anyhow::bail!(
                "multiple local frontends are live ({}); pass `--mount <NAME>` to choose one",
                mounts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn select_named(&self, selector: &str) -> Result<ShellTarget> {
        if let Some(mount) = self
            .0
            .iter()
            .find(|mount| mount.path() == Path::new(selector))
        {
            return Ok(ShellTarget::Local(mount.path().to_path_buf()));
        }

        let matches = self
            .0
            .iter()
            .filter(|mount| mount.has_name(selector))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [mount] => Ok(ShellTarget::Local(mount.path().to_path_buf())),
            [] => anyhow::bail!(
                "no local frontend matches `--mount {selector}`; available mount paths: {}",
                self.listed()
            ),
            _ => anyhow::bail!(
                "mount name `{selector}` is ambiguous; pass an exact path: {}",
                matches
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn listed(&self) -> String {
        if self.0.is_empty() {
            "(none)".to_string()
        } else {
            self.0
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

fn sorted_unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Which rc lever, if any, omnifs can use to inject its prompt.
enum ShellKind {
    Zsh,
    Bash,
    Other,
}

impl ShellKind {
    fn detect(shell: &Path) -> Self {
        match shell.file_name().and_then(|name| name.to_str()) {
            Some(name) if name.contains("zsh") => Self::Zsh,
            Some(name) if name.contains("bash") => Self::Bash,
            _ => Self::Other,
        }
    }
}

fn resolve_shell(override_shell: Option<&str>) -> PathBuf {
    if let Some(shell) = override_shell {
        return PathBuf::from(shell);
    }
    std::env::var_os("SHELL").map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from)
}

/// The user's real `ZDOTDIR` (or `$HOME`), so the omnifs zsh rc can source their
/// config back in after we redirect `ZDOTDIR` at our own dir.
fn prev_zdotdir() -> PathBuf {
    std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_default()
}

fn apply_context_env(cmd: &mut Command, mount_point: &Path, mounts: &[MountInfo], hermetic: bool) {
    cmd.env("OMNIFS_IN_SHELL", "1");
    cmd.env(OMNIFS_MOUNT_POINT_ENV, mount_point);
    cmd.env("OMNIFS_MOUNTS", mounts_env(mounts));
    if hermetic {
        cmd.env("OMNIFS_HERMETIC", "1");
    }
}

/// `mount=provider;mount=provider` for the rc to parse into its prompt map. Mount
/// names and provider names are validated identifiers, so neither carries `;`/`=`.
fn mounts_env(mounts: &[MountInfo]) -> String {
    mounts
        .iter()
        .map(|m| format!("{}={}", m.mount, m.provider_name))
        .collect::<Vec<_>>()
        .join(";")
}

fn set_cwd_to_mount(cmd: &mut Command, mount_point: &Path) {
    if mount_point.is_dir() {
        cmd.current_dir(mount_point);
    }
}

/// Hand the terminal to `cmd` and forward its exit code. A clean (0) or
/// signal-terminated exit returns `Ok`; a non-zero exit becomes this process's
/// exit code so one-shot commands stay scriptable.
fn spawn_and_propagate(mut cmd: Command, context: String) -> Result<()> {
    let status = cmd.status().with_context(|| context)?;
    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => {
            // `shell` forwards the inner command's code and exits here rather
            // than returning to `main`, so record its usage at this exit site.
            crate::telemetry::record_cli_exit("shell", code);
            std::process::exit(code)
        },
    }
}

const ZSH_ZSHENV: &str = r#"# omnifs shell: inherit the user's zshenv (PATH, etc.) before anything else.
[[ -z "$OMNIFS_HERMETIC" && -r "${OMNIFS_PREV_ZDOTDIR:-$HOME}/.zshenv" ]] && \
  source "${OMNIFS_PREV_ZDOTDIR:-$HOME}/.zshenv"
"#;

const ZSH_ZSHRC: &str = r#"# omnifs shell: inherit the user's zsh config, then take over the prompt.

if [[ -z "$OMNIFS_HERMETIC" && -r "${OMNIFS_PREV_ZDOTDIR:-$HOME}/.zshrc" ]]; then
  source "${OMNIFS_PREV_ZDOTDIR:-$HOME}/.zshrc"
fi

# A user prompt framework (starship, powerlevel10k, ...) re-scans the cwd every
# render; on a lazy projection each scan is a provider round-trip. Drop inherited
# prompt hooks and build a prompt from $PWD plus the mount->provider map passed
# in via OMNIFS_MOUNTS.
autoload -Uz add-zsh-hook
precmd_functions=()
(( ${+functions[precmd]} )) && unfunction precmd

typeset -gA _omnifs_providers
() {
  local pair
  for pair in ${(s.;.)OMNIFS_MOUNTS}; do
    [[ -n "$pair" ]] && _omnifs_providers[${pair%%=*}]="${pair#*=}"
  done
}

_omnifs_precmd() {
  local seg=omnifs
  if [[ "$PWD/" == "${OMNIFS_MOUNT_POINT}"/* ]]; then
    local rel=${PWD#$OMNIFS_MOUNT_POINT}; rel=${rel#/}
    local mount=${rel%%/*}
    if [[ -n "$mount" ]]; then
      seg=$mount
      local provider=${_omnifs_providers[$mount]}
      [[ -n "$provider" && "$provider" != "$mount" ]] && seg="$mount ($provider)"
    fi
  fi
  _OMNIFS_SEG=$seg
}
add-zsh-hook precmd _omnifs_precmd

setopt PROMPT_SUBST
PROMPT='%F{magenta}omnifs:${_OMNIFS_SEG}%f %F{blue}%~%f %# '
"#;

const BASH_RC: &str = r#"# omnifs shell: inherit the user's bash config, then take over the prompt.

if [[ -z "$OMNIFS_HERMETIC" && -r "$HOME/.bashrc" ]]; then
  source "$HOME/.bashrc"
fi

# Replace any inherited PROMPT_COMMAND (starship, etc.) so the prompt is built
# only from $PWD plus the mount->provider map passed in via OMNIFS_MOUNTS, never
# from a per-render filesystem scan.
declare -A _omnifs_providers
IFS=';' read -ra _omnifs_pairs <<< "$OMNIFS_MOUNTS"
for _omnifs_pair in "${_omnifs_pairs[@]}"; do
  [[ -n "$_omnifs_pair" ]] && _omnifs_providers["${_omnifs_pair%%=*}"]="${_omnifs_pair#*=}"
done

_omnifs_prompt() {
  local seg=omnifs
  case "$PWD/" in
    "$OMNIFS_MOUNT_POINT"/*)
      local rel=${PWD#$OMNIFS_MOUNT_POINT}; rel=${rel#/}
      local mount=${rel%%/*}
      if [[ -n "$mount" ]]; then
        seg=$mount
        local provider=${_omnifs_providers[$mount]}
        [[ -n "$provider" && "$provider" != "$mount" ]] && seg="$mount ($provider)"
      fi
      ;;
  esac
  PS1="\[\e[35m\]omnifs:${seg}\[\e[0m\] \[\e[34m\]\w\[\e[0m\] \$ "
}
PROMPT_COMMAND=_omnifs_prompt
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use omnifs_api::{DaemonBackend, DaemonHealth, MountInfo};

    use crate::cli::Cli;

    fn status_with_mounts(names: &[&str]) -> DaemonStatus {
        DaemonStatus {
            version: "test".to_string(),
            api_major: 0,
            api_minor: 0,
            pid: 0,
            instance_id: String::new(),
            executable: PathBuf::new(),
            mount_point: PathBuf::new(),
            config_dir: PathBuf::new(),
            cache_dir: PathBuf::new(),
            providers_dir: PathBuf::new(),
            frontends: Vec::new(),
            backend: DaemonBackend::default(),
            mounts: names
                .iter()
                .map(|name| MountInfo {
                    mount: (*name).to_string(),
                    provider_name: "provider".to_string(),
                    provider_id: "id".to_string(),
                    auth_health: None,
                })
                .collect(),
            failed: Vec::new(),
            health: DaemonHealth::default(),
        }
    }

    #[test]
    fn local_mounts_normalize_zero_one_and_multiple_candidates() {
        assert!(
            LocalMounts::from_paths(Vec::<PathBuf>::new())
                .select(None, false)
                .is_err()
        );

        let one = LocalMounts::from_paths([PathBuf::from("/tmp/omnifs")]);
        assert_eq!(
            one.select(None, false).unwrap(),
            ShellTarget::Local(PathBuf::from("/tmp/omnifs"))
        );

        let many = LocalMounts::from_paths([
            PathBuf::from("/tmp/b"),
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/a"),
        ]);
        let error = many.select(None, false).unwrap_err().to_string();
        assert!(error.contains("multiple local frontends"));
        assert!(error.contains("--mount"));
        assert!(error.contains("/tmp/a, /tmp/b"));
    }

    #[test]
    fn explicit_selector_accepts_unique_basename_or_exact_path() {
        let mounts =
            LocalMounts::from_paths([PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")]);
        assert_eq!(
            mounts.select(Some("two"), false).unwrap(),
            ShellTarget::Local(PathBuf::from("/tmp/two"))
        );
        assert_eq!(
            mounts.select(Some("/tmp/one"), false).unwrap(),
            ShellTarget::Local(PathBuf::from("/tmp/one"))
        );
    }

    #[test]
    fn explicit_selector_rejects_invalid_and_ambiguous_names() {
        let mounts = LocalMounts::from_paths([
            PathBuf::from("/tmp/one/mount"),
            PathBuf::from("/tmp/two/mount"),
        ]);
        let ambiguous = mounts.select(Some("mount"), false).unwrap_err().to_string();
        assert!(ambiguous.contains("ambiguous"));
        assert!(ambiguous.contains("/tmp/one/mount"));
        assert!(ambiguous.contains("/tmp/two/mount"));

        let invalid = mounts
            .select(Some("missing"), false)
            .unwrap_err()
            .to_string();
        assert!(invalid.contains("no local frontend matches"));
        assert!(invalid.contains("/tmp/one/mount"));
    }

    #[test]
    fn explicit_selector_bypasses_a_guest_target() {
        let context = ShellContext::new(None, Vec::new());
        let target = context
            .choose_target(
                Some(LocalMounts::from_paths([PathBuf::from("/tmp/omnifs")])),
                Some("omnifs"),
                false,
                Some(ShellTarget::Krunkit),
            )
            .unwrap();
        assert_eq!(target, ShellTarget::Local(PathBuf::from("/tmp/omnifs")));
    }

    #[test]
    fn root_mount_roster_is_sorted_and_deduplicated_with_configured_fallback() {
        let live = ShellContext::new(
            Some(status_with_mounts(&["zeta", "alpha", "alpha"])),
            vec!["fallback".to_string()],
        );
        assert_eq!(
            live.root_mounts(),
            vec!["alpha".to_string(), "zeta".to_string()]
        );

        let fallback = ShellContext::new(
            None,
            vec!["zeta".to_string(), "alpha".to_string(), "alpha".to_string()],
        );
        assert_eq!(
            fallback.root_mounts(),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }

    #[test]
    fn interactive_banner_lists_root_mounts() {
        let context = ShellContext::new(Some(status_with_mounts(&["zeta", "alpha"])), vec![]);
        let banner = context.banner("local", Path::new("/tmp/omnifs"));
        assert!(banner.contains("omnifs shell (local) at /tmp/omnifs"));
        assert!(banner.contains("root mounts: alpha, zeta"));
    }

    #[test]
    fn offline_banner_does_not_call_configured_roots_live_mounts() {
        let context = ShellContext::new(None, vec!["alpha".to_string()]);
        let banner = context.banner("local", Path::new("/tmp/omnifs"));
        assert!(banner.contains("configured roots (unverified): alpha"));
        assert!(!banner.contains("root mounts: alpha"));
    }

    #[test]
    fn shell_mount_flag_parses_before_trailing_command() {
        let cli =
            Cli::try_parse_from(["omnifs", "shell", "--mount", "omnifs", "--", "pwd"]).unwrap();
        match cli.command {
            Some(crate::cli::Commands::Shell(args)) => {
                assert_eq!(args.mount.as_deref(), Some("omnifs"));
                assert_eq!(args.command, vec!["pwd".to_string()]);
            },
            _ => panic!("expected shell command"),
        }

        let command = Cli::command();
        let mut shell = command
            .find_subcommand("shell")
            .expect("shell command")
            .clone();
        assert!(
            shell
                .get_arguments()
                .any(|argument| argument.get_long() == Some("mount"))
        );
        assert!(shell.render_help().to_string().contains("--mount <NAME>"));
    }
}
