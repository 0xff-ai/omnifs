//! Final paths + per-mount summary for the setup command.
//!
//! Renders to a `String` so the shape stays testable without a TTY.

use std::collections::BTreeMap;
use std::fmt::{self, Display};

use omnifs_home::WorkspaceLayout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub provider_id: String,
    pub mount_name: String,
    pub outcome: Result<(), String>,
}

pub struct SetupSummary<'a> {
    paths: &'a WorkspaceLayout,
    mount_label: &'a str,
    mount_root: &'a str,
    browse_hint: &'a str,
    configured: &'a BTreeMap<String, String>,
    results: &'a [InitResult],
}

impl<'a> SetupSummary<'a> {
    pub fn new(
        paths: &'a WorkspaceLayout,
        mount_label: &'a str,
        mount_root: &'a str,
        browse_hint: &'a str,
        configured: &'a BTreeMap<String, String>,
        results: &'a [InitResult],
    ) -> Self {
        Self {
            paths,
            mount_label,
            mount_root,
            browse_hint,
            configured,
            results,
        }
    }

    fn any_ready(&self) -> bool {
        !self.configured.is_empty() || self.results.iter().any(|r| r.outcome.is_ok())
    }
}

impl Display for SetupSummary<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(out)?;
        writeln!(out, "Setup summary")?;
        writeln!(
            out,
            "  Mounts dir       {}",
            self.paths.mounts_dir.display()
        )?;
        writeln!(
            out,
            "  Credentials      {}",
            self.paths.credentials_file.display()
        )?;
        writeln!(
            out,
            "  Providers dir    {}",
            self.paths.providers_dir.display()
        )?;
        writeln!(
            out,
            "  {:<16} {}   (after `up`)",
            self.mount_label, self.mount_root
        )?;

        if !self.configured.is_empty() || !self.results.is_empty() {
            writeln!(out, "\nMounts")?;
        }
        for (provider_id, mount_name) in self.configured {
            writeln!(
                out,
                "  {}/{mount_name}   ({provider_id}, already configured)",
                self.mount_root
            )?;
        }
        for result in self.results {
            let mount_path = format!("{}/{}", self.mount_root, result.mount_name);
            match &result.outcome {
                Ok(()) => {
                    writeln!(out, "  {mount_path}   ({})   ok", result.provider_id)?;
                },
                Err(reason) => {
                    writeln!(
                        out,
                        "  {} ({})   failed: {reason}",
                        result.mount_name, result.provider_id
                    )?;
                    writeln!(out, "    retry with `omnifs init {}`", result.provider_id)?;
                },
            }
        }

        if self.any_ready() {
            writeln!(
                out,
                "\nNext: {} to browse, or `omnifs status` for runtime state.",
                self.browse_hint
            )?;
        }
        Ok(())
    }
}
