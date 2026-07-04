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
//! Backend-aware: the daemon's mode and mount point come from the run-state
//! file `omnifs up` writes (`<config_dir>/launch.json`). The host-native
//! backend's mount is host-visible, so the subshell above runs on the
//! host pointed at it. The Docker backend's mount lives inside the container at
//! the guest mount path and is invisible on the host, so there `omnifs shell`
//! execs into the running container instead.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;
use omnifs_api::MountInfo;

use crate::launch_record::LaunchRecord;
use crate::session::GUEST_MOUNT;
use crate::workspace::Workspace;
use omnifs_home::OMNIFS_MOUNT_POINT_ENV;

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

        // The run-state file is the source of truth for whether a daemon was
        // started and how (native vs container) plus its mount point — no live
        // daemon required to discover the mode.
        let record = LaunchRecord::read(&paths.config_dir)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no omnifs run-state file in {}; start the daemon with `omnifs up`, \
                 then `omnifs shell`",
                paths.config_dir.display()
            )
        })?;
        // The Docker backend's mount lives inside the container, not on the
        // host, so a host subshell can never see it; exec into the container.
        // The host-native backend falls through to the host subshell below.
        if let Some(container) = record.container_name() {
            return self.exec_in_container(container);
        }

        let mode = record.mode_label();

        // A live status call, when the daemon answers, supplies the mount→provider
        // map for the prompt and the canonical mount point; if it does not, fall
        // back to the record and warn that the mount may be stale.
        let live = workspace.daemon().status().await.ok();
        if live.is_none() {
            anstream::eprintln!(
                "note: the daemon is not answering; its mount may be stale (try `omnifs up`)"
            );
        }
        let mount_point = live
            .as_ref()
            .map(|status| status.mount_point.clone())
            .or_else(|| record.mount_point().map(Path::to_path_buf))
            .ok_or_else(|| {
                anyhow::anyhow!("run-state file has no mount point; rerun `omnifs up`")
            })?;
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

        anstream::println!(
            "omnifs shell ({mode}) at {} (type `exit` to leave)",
            mount_point.display()
        );
        spawn_and_propagate(cmd, "launch omnifs shell".to_string())
    }

    /// Attach to the Docker-backend daemon by `docker exec`'ing into its
    /// container, landing in the projected tree. The container ships its own
    /// omnifs-tuned zsh rc, so no host rc plumbing applies here; `--shell`
    /// overrides the default and a trailing command runs non-interactively.
    fn exec_in_container(&self, container: &str) -> Result<()> {
        let mut cmd = Command::new("docker");
        cmd.arg("exec").arg("-i");
        if std::io::stdin().is_terminal() {
            cmd.arg("-t");
        }
        cmd.arg("-w").arg(GUEST_MOUNT);
        cmd.arg(container);
        if self.command.is_empty() {
            cmd.arg(self.shell.as_deref().unwrap_or("/bin/zsh"));
            anstream::println!("omnifs shell (container) at {GUEST_MOUNT} (type `exit` to leave)");
        } else {
            cmd.args(&self.command);
        }
        spawn_and_propagate(cmd, format!("open shell in container `{container}`"))
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
/// names and provider ids are validated identifiers, so neither carries `;`/`=`.
fn mounts_env(mounts: &[MountInfo]) -> String {
    mounts
        .iter()
        .map(|m| format!("{}={}", m.mount, m.provider_id))
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
