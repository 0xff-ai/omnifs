use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct ChangelogSection {
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Changelog {
    pub raw: String,
    pub preamble: String,
    pub unreleased_body: String,
    sections: Vec<ChangelogSection>,
}

impl Changelog {
    pub fn parse(raw: String) -> Result<Self> {
        let mut preamble = String::new();
        let mut unreleased_body = String::new();
        let mut sections = Vec::new();

        let mut current_heading: Option<String> = None;
        let mut current_body = String::new();
        let mut seen_unreleased = false;

        for line in raw.lines() {
            if let Some(title) = line.strip_prefix("## [") {
                if let Some(heading) = current_heading.take() {
                    sections.push(ChangelogSection {
                        heading,
                        body: std::mem::take(&mut current_body),
                    });
                }

                let heading = format!("## [{title}");
                if title.starts_with("Unreleased]") {
                    seen_unreleased = true;
                    current_heading = Some(heading);
                    continue;
                }
                current_heading = Some(heading);
                continue;
            }

            if current_heading.is_none() {
                preamble.push_str(line);
                preamble.push('\n');
            } else if current_heading
                .as_ref()
                .is_some_and(|h| h.starts_with("## [Unreleased]"))
            {
                unreleased_body.push_str(line);
                unreleased_body.push('\n');
            } else {
                current_body.push_str(line);
                current_body.push('\n');
            }
        }

        if let Some(heading) = current_heading {
            if heading.starts_with("## [Unreleased]") {
                // body already captured
            } else {
                sections.push(ChangelogSection {
                    heading,
                    body: current_body,
                });
            }
        }

        if !seen_unreleased {
            bail!("CHANGELOG.md must contain a ## [Unreleased] section");
        }

        Ok(Self {
            raw,
            preamble,
            unreleased_body,
            sections,
        })
    }

    pub fn has_unreleased_section(&self) -> bool {
        self.raw.contains("## [Unreleased]")
    }

    pub fn section_for_version(&self, version: &str) -> Option<&ChangelogSection> {
        let needle = format!("## [{version}]");
        self.sections
            .iter()
            .find(|section| section.heading.starts_with(&needle))
    }

    pub fn section_body_for_version(&self, version: &str) -> Option<String> {
        self.section_for_version(version)
            .map(|section| section.body.trim().to_string())
    }
}

pub fn unreleased_has_content(log: &Changelog) -> bool {
    log.unreleased_body.trim().lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with("### ")
    })
}

pub fn finalize_unreleased(log: &mut Changelog, version: &str) -> Result<()> {
    if !unreleased_has_content(log) {
        bail!("CHANGELOG.md [Unreleased] has no release note bullets");
    }

    let date = {
        let date = time::OffsetDateTime::now_utc().date();
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        )
    };

    let released_heading = format!("## [{version}] - {date}");
    let mut rebuilt = String::new();
    rebuilt.push_str(log.preamble.trim_end());
    rebuilt.push_str("\n\n## [Unreleased]\n\n");
    rebuilt.push_str(&released_heading);
    rebuilt.push('\n');
    rebuilt.push_str(log.unreleased_body.trim_end());
    rebuilt.push_str("\n\n");

    for section in &log.sections {
        rebuilt.push_str(&section.heading);
        rebuilt.push('\n');
        rebuilt.push_str(&section.body);
        if !section.body.ends_with('\n') {
            rebuilt.push('\n');
        }
        rebuilt.push('\n');
    }

    log.raw = rebuilt.trim_end().to_string();
    log.raw.push('\n');
    let raw = log.raw.clone();
    *log = Changelog::parse(raw)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unreleased_and_version_sections() {
        let raw = r#"# Changelog

## [Unreleased]

### Added

- New thing

## [0.1.0] - 2026-01-01

### Fixed

- Bug
"#;
        let log = Changelog::parse(raw.to_string()).unwrap();
        assert!(unreleased_has_content(&log));
        assert_eq!(
            log.section_body_for_version("0.1.0").unwrap(),
            "### Fixed\n\n- Bug"
        );
    }

    #[test]
    fn finalize_moves_unreleased_into_version_section() {
        let raw = r#"# Changelog

## [Unreleased]

### Added

- Feature

## [0.1.0] - 2026-01-01

### Fixed

- Bug
"#;
        let mut log = Changelog::parse(raw.to_string()).unwrap();
        finalize_unreleased(&mut log, "0.2.0").unwrap();
        assert!(log.section_for_version("0.2.0").is_some());
        assert!(!unreleased_has_content(&log));
        assert!(log.raw.contains("## [Unreleased]"));
    }
}
