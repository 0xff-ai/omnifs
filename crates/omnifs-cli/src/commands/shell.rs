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
//! instead; more than one live local mount with no guest attached is
//! reported as an ambiguity rather than silently picked for the user.

use std::path::{Path, PathBuf};
use std::process::Command;

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

#[derive(Args, Debug, Clone, Default)]
pub struct ShellArgs {
    /// Start a clean shell that does not source your shell rc files.
    #[arg(long)]
    pub hermetic: bool,
    /// Shell to launch (defaults to `$SHELL`).
    #[arg(long)]
    pub shell: Option<String>,
    /// Run a command in the mount context instead of an interactive shell.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

impl ShellArgs {
    pub async fn run(self) -> Result<()> {
        if std::env::var_os("OMNIFS_IN_SHELL").is_some() {
            anstream::eprintln!(
                "note: already inside an omnifs shell; opening a nested one (exit twice to return)"
            );
        }

        let workspace = Workspace::resolve()?;
        let paths = workspace.layout();

        // A guest frontend's mount is invisible on the host, so prefer a live
        // one over a host subshell; probe rather than trust a single recorded
        // choice, since several frontends can be configured at once.
        if docker_frontend_is_running(paths).await {
            let container_name = crate::frontend_container::frontend_container_name(paths)?;
            return self.exec_in_container(&container_name);
        }
        if krunkit_frontend_is_running(paths).await {
            return self.exec_in_krunkit_guest(paths);
        }

        // No guest frontend is live: fall back to a local mount. A live
        // status call, when the daemon answers, supplies the mount→provider
        // map for the prompt and the authoritative local attachment set; if
        // it does not answer, discover local mounts from the runner-owned
        // mount-state records directly and warn that they may be stale.
        let live = workspace.daemon().status().await.ok();
        if live.is_none() {
            anstream::eprintln!(
                "note: the daemon is not answering; its mount may be stale (try `omnifs up`)"
            );
        }
        let mount_point = select_local_mount(paths, live.as_ref())?;
        let mounts = live.map(|status| status.mounts).unwrap_or_default();

        // A one-shot command runs directly in the mount context; no rc or prompt
        // tuning is needed for a non-interactive invocation.
        if !self.command.is_empty() {
            let mut cmd = Command::new(&self.command[0]);
            cmd.args(&self.command[1..]);
            apply_context_env(&mut cmd, &mount_point, &mounts, self.hermetic);
            set_cwd_to_mount(&mut cmd, &mount_point);
            return spawn_and_propagate(cmd, format!("run `{}`", self.command[0]));
        }

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
        apply_context_env(&mut cmd, &mount_point, &mounts, self.hermetic);
        set_cwd_to_mount(&mut cmd, &mount_point);

        anstream::eprintln!(
            "omnifs shell (local) at {} (type `exit` to leave)",
            mount_point.display()
        );
        spawn_and_propagate(cmd, "launch omnifs shell".to_string())
    }

    /// Attach to the optional FUSE frontend by execing into it, landing in
    /// the projected tree. The minimal frontend image ships only `/bin/sh`,
    /// so no host rc plumbing applies here; `--shell` overrides the default
    /// and a trailing command runs non-interactively.
    ///
    /// Uses [`DockerBackend`] only for command construction; the image field of
    /// its `DockerTarget`
    /// is unused here, so the dev placeholder is fine regardless of build
    /// channel, mirroring `frontend down`/`frontend status`.
    fn exec_in_container(&self, container_name: &ContainerName) -> Result<()> {
        let target = DockerTarget::new(
            container_name.as_str().to_string(),
            FRONTEND_DEV_IMAGE.to_string(),
        )?;
        let backend = DockerBackend::new(Runtime::connect_for(&target)?);
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        if self.command.is_empty() {
            anstream::eprintln!("omnifs shell (container) at {GUEST_MOUNT} (type `exit` to leave)");
        }
        spawn_and_propagate(cmd, format!("open shell in container `{container_name}`"))
    }

    /// Attach to the krunkit guest over ssh-over-vsock, landing in the
    /// projected tree. `shell_command` is pure construction (no I/O), so the
    /// `socat` probe (an I/O check) happens here, at the one call site about
    /// to actually run it.
    fn exec_in_krunkit_guest(&self, paths: &WorkspaceLayout) -> Result<()> {
        krunkit_backend::ensure_socat_available()?;
        let backend = KrunkitBackend::new(paths.config_dir.clone());
        let cmd = backend.shell_command(self.shell.as_deref(), &self.command);
        if self.command.is_empty() {
            anstream::eprintln!("omnifs shell (krunkit) at {GUEST_MOUNT} (type `exit` to leave)");
        }
        spawn_and_propagate(cmd, "open shell in the krunkit guest".to_string())
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

/// The live local mount points: the daemon's own attachment list when it
/// answers, else the runner-owned mount-state records on disk (the daemon
/// may be down or unreachable, but a local frontend runner persists its own
/// state independently).
fn local_mount_candidates(
    paths: &WorkspaceLayout,
    live: Option<&DaemonStatus>,
) -> Result<Vec<PathBuf>> {
    if let Some(status) = live {
        return Ok(status
            .frontends
            .iter()
            .filter(|frontend| frontend.delivery == FrontendDelivery::Local)
            .map(|frontend| frontend.mount_point.clone())
            .collect());
    }
    let mut candidates = Vec::new();
    for path in MountState::files_under(&paths.frontend_state_root())
        .context("discover local frontend records")?
    {
        match MountState::read_file(&path) {
            Ok(state) => {
                // A record whose mount is no longer live is teardown debris,
                // not a shell destination; counting it would also turn one
                // live mount plus one stale record into a false ambiguity.
                // Without the daemon feature the ownership probe (an
                // omnifs-nfs dependency) does not exist, so records are
                // trusted as-is there.
                #[cfg(feature = "daemon")]
                if !crate::host_teardown::local_mount_is_owned(&state) {
                    continue;
                }
                candidates.push(state.mount_point);
            },
            Err(error) => {
                anstream::eprintln!(
                    "⚠  Skipping local frontend record {}: {error}",
                    path.display()
                );
            },
        }
    }
    Ok(candidates)
}

/// Pick the local mount `omnifs shell` should enter. Ambiguity — more than
/// one live local mount with no guest frontend attached — is never silently
/// resolved for the user; it is a distinct, equally-preferred choice, so the
/// caller is asked to `cd` into one directly instead.
fn select_local_mount(paths: &WorkspaceLayout, live: Option<&DaemonStatus>) -> Result<PathBuf> {
    let mut candidates = local_mount_candidates(paths, live)?;
    match candidates.len() {
        0 => anyhow::bail!("no host mount is available; start it with `omnifs frontend up`"),
        1 => Ok(candidates.remove(0)),
        _ => {
            candidates.sort();
            let listed = candidates
                .iter()
                .map(|mount_point| WorkspaceLayout::display(mount_point))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple local frontends are live ({listed}); `cd` into the one you want instead of running `omnifs shell`"
            )
        },
    }
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
