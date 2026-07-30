use anyhow::anyhow;
use omnifs_api::{MountRecord, ProviderMetadata};
use omnifs_core::MountName;

pub(crate) fn select(
    embedded: &[ProviderMetadata],
    provider_arg: Option<&str>,
    interactive: bool,
    output: &crate::ui::output::Output,
) -> anyhow::Result<String> {
    if let Some(provider) = provider_arg {
        return Ok(provider.to_owned());
    }
    if !interactive {
        anyhow::bail!("non-interactive mode requires a provider path, digest, or embedded name");
    }
    let mut providers = embedded
        .iter()
        .map(|provider| provider.reference.name.clone())
        .collect::<Vec<_>>();
    providers.sort();
    if providers.is_empty() {
        anyhow::bail!("the embedded provider bundle contains no providers");
    }
    crate::ui::prompt::Select::new("Which provider?")
        .items(providers)
        .ask_with_output(output)
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
        anyhow::bail!(
            "mount `{proposed}` already exists; pass --name explicitly (suggested: `{suggestion}`)"
        );
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
