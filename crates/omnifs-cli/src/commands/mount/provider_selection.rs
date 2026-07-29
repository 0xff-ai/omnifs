use anyhow::anyhow;
use omnifs_api::MountRecord;
use omnifs_core::MountName;

use crate::error::WithHint;

/// Resolve the provider selector for `mount add`. The only interactive
/// "Which provider?" prompt lives in `create.rs`'s aligned catalog picker,
/// which runs before this and supplies its pick as `provider_arg`; this
/// function's job is purely to validate that a selector ended up chosen,
/// never to prompt for one itself.
pub(crate) fn select(provider_arg: Option<&str>) -> anyhow::Result<String> {
    provider_arg.map(str::to_owned).ok_or_else(|| {
        anyhow!("non-interactive mode requires a provider path, digest, or embedded name")
    })
}

pub(crate) fn mount_name(
    mounts: &[MountRecord],
    default_mount: &str,
    explicit_name: Option<&str>,
    interactive: bool,
    yes: bool,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<MountName> {
    let proposed = explicit_name.map_or_else(|| default_mount.to_owned(), str::to_owned);
    let proposed_name = MountName::new(proposed.as_str())?;

    // Explicit names are always returned as requested; the caller applies
    // the create-only collision check before auth or config side effects.
    // Accidental default-name collisions still go through the unique-name
    // flow below.
    if explicit_name.is_some() {
        return Ok(proposed_name);
    }

    let name = ensure_unique_name(mounts, proposed_name, interactive, yes, output, key_width)?;
    Ok(name)
}

fn ensure_unique_name(
    mounts: &[MountRecord],
    proposed: MountName,
    interactive: bool,
    yes: bool,
    output: &crate::ui::output::Output,
    key_width: usize,
) -> anyhow::Result<MountName> {
    if !mounts.iter().any(|mount| mount.definition.name == proposed) {
        return Ok(proposed);
    }
    let suggestion = next_available(mounts, &proposed)?;
    // `--yes` accepts the auto-suggested name on collision, even
    // non-interactively (it never overwrites the existing mount).
    if yes {
        output.ledger_row(
            &crate::ui::render::LedgerRow::new(
                crate::ui::style::Glyph::Warn,
                "mount name",
                format!("{proposed} taken, using {suggestion}"),
            ),
            key_width,
        );
        return Ok(suggestion);
    }
    if !interactive {
        return Err(anyhow!("mount `{proposed}` already exists"))
            .with_hint(format!("pass --name {suggestion}"));
    }
    let name = crate::ui::prompt::Text::new("New mount name")
        .with_default(suggestion.as_str())
        .ask_with_output(output)?;
    Ok(MountName::new(name)?)
}

fn next_available(mounts: &[MountRecord], base: &MountName) -> anyhow::Result<MountName> {
    (2..1000)
        .filter_map(|n| MountName::new(format!("{base}-{n}")).ok())
        .find(|candidate| {
            !mounts
                .iter()
                .any(|mount| mount.definition.name == *candidate)
        })
        .ok_or_else(|| anyhow!("could not find an available mount name derived from `{base}`"))
}
