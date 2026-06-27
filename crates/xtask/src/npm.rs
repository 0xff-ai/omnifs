use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const ROOT_PACKAGE: &str = "@0xff-ai/omnifs";

/// One platform entry from `npm/platforms.json`. Extra keys (e.g. `runner`)
/// are ignored; only the fields the sync/validate policy reads are modeled.
#[derive(Deserialize)]
struct PlatformSpec {
    package: String,
    #[serde(rename = "rustTarget")]
    rust_target: String,
    os: String,
    cpu: String,
}

type Catalog = BTreeMap<String, PlatformSpec>;

#[derive(Deserialize)]
struct PackageJson {
    #[serde(skip)]
    path: PathBuf,
    name: String,
    version: String,
    os: Option<Vec<String>>,
    cpu: Option<Vec<String>>,
    #[serde(rename = "optionalDependencies", default)]
    optional_dependencies: BTreeMap<String, String>,
}

struct NpmLayout {
    root_package: PackageJson,
    platform_packages: BTreeMap<String, PackageJson>,
}

/// Sync the root, platform, and root `optionalDependencies` versions via
/// `npm pkg set`, which preserves each manifest's key order rather than
/// reserializing JSON.
pub fn sync(root: &Path, version: &str) -> Result<()> {
    let catalog = load_catalog(root)?;
    let layout = discover_layout(root, &catalog)?;
    write_version(root, &layout.root_package, version)?;
    for pkg in layout.platform_packages.values() {
        write_version(root, pkg, version)?;
    }
    println!("synced npm packages to version {version}");
    Ok(())
}

/// Validate platform metadata, root `optionalDependencies`, the cargo-dist
/// macOS target set, and the inlined runtime platform map. On any mismatch,
/// print every error and exit non-zero.
pub fn validate(root: &Path, version: &str) -> Result<()> {
    let catalog = match load_catalog(root) {
        Ok(catalog) => catalog,
        Err(error) => print_errors_and_exit("npm platform validation", &[format!("{error:#}")]),
    };
    let layout = match discover_layout(root, &catalog) {
        Ok(layout) => layout,
        Err(error) => print_errors_and_exit("npm platform validation", &[format!("{error:#}")]),
    };

    let mut errors = Vec::new();
    validate_packages(version, &catalog, &layout, &mut errors);
    validate_root_optional_dependencies(version, &layout, &catalog, &mut errors);
    validate_dist_targets(root, &catalog, &mut errors);
    validate_resolve_binary_inline(root, &catalog, &mut errors);

    if !errors.is_empty() {
        print_errors_and_exit("npm platform validation", &errors);
    }
    println!("validated {} npm platform entries", catalog.len());
    Ok(())
}

fn load_catalog(root: &Path) -> Result<Catalog> {
    let path = root.join("npm/platforms.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn discover_layout(root: &Path, catalog: &Catalog) -> Result<NpmLayout> {
    let root_package = load_package_json(root.join("npm/omnifs/package.json"))?;
    let platform_dir = root.join("npm/platform");
    let mut platform_packages = BTreeMap::new();

    for platform in catalog.keys() {
        let path = platform_dir.join(platform).join("package.json");
        if !path.exists() {
            bail!("missing npm platform package at {}", path.display());
        }
        platform_packages.insert(platform.clone(), load_package_json(path)?);
    }

    for entry in platform_dir_entries(&platform_dir)? {
        if !catalog.contains_key(&entry) {
            bail!("npm/platform/{entry} is not declared in npm/platforms.json");
        }
    }

    Ok(NpmLayout {
        root_package,
        platform_packages,
    })
}

fn load_package_json(path: PathBuf) -> Result<PackageJson> {
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut pkg: PackageJson =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    pkg.path = path;
    Ok(pkg)
}

fn write_version(root: &Path, pkg: &PackageJson, version: &str) -> Result<()> {
    let mut args = vec![
        "pkg".to_string(),
        "set".to_string(),
        format!("version={version}"),
    ];
    if pkg.name == ROOT_PACKAGE {
        for dep in pkg.optional_dependencies.keys() {
            args.push(format!("optionalDependencies.{dep}={version}"));
        }
    }
    let dir = pkg
        .path
        .parent()
        .with_context(|| format!("{} has no parent directory", pkg.path.display()))?;
    let status = Command::new("npm")
        .args(&args)
        .arg("--prefix")
        .arg(dir)
        .current_dir(root)
        .status()
        .context("run `npm pkg set`")?;
    if !status.success() {
        bail!("npm pkg set failed for {}", pkg.path.display());
    }
    Ok(())
}

fn validate_packages(
    version: &str,
    catalog: &Catalog,
    layout: &NpmLayout,
    errors: &mut Vec<String>,
) {
    for (platform, spec) in catalog {
        let Some(pkg) = layout.platform_packages.get(platform) else {
            errors.push(format!(
                "missing npm platform package directory for {platform}"
            ));
            continue;
        };
        let path = pkg.path.display();
        if pkg.name != spec.package {
            errors.push(format!(
                "{path} name {} != platforms.json package {}",
                pkg.name, spec.package
            ));
        }
        if !single_eq(&pkg.os, &spec.os) {
            errors.push(format!("{path} os mismatch for {platform}"));
        }
        if !single_eq(&pkg.cpu, &spec.cpu) {
            errors.push(format!("{path} cpu mismatch for {platform}"));
        }
        if pkg.version != version {
            errors.push(format!(
                "{path} version {} != Cargo.toml workspace version {version}",
                pkg.version
            ));
        }
    }

    if layout.root_package.version != version {
        errors.push(format!(
            "{} version {} != Cargo.toml workspace version {version}",
            layout.root_package.path.display(),
            layout.root_package.version
        ));
    }
}

fn validate_root_optional_dependencies(
    version: &str,
    layout: &NpmLayout,
    catalog: &Catalog,
    errors: &mut Vec<String>,
) {
    let actual: Vec<String> = layout
        .root_package
        .optional_dependencies
        .keys()
        .cloned()
        .collect();
    let expected: Vec<String> = catalog.values().map(|spec| spec.package.clone()).collect();
    assert_set_equal(
        &actual,
        &expected,
        "npm/omnifs/package.json optionalDependencies",
        errors,
    );
    for (dep, dep_version) in &layout.root_package.optional_dependencies {
        if dep_version != version {
            errors.push(format!(
                "{} optionalDependencies.{dep} version {dep_version} != Cargo.toml workspace version {version}",
                layout.root_package.path.display()
            ));
        }
    }
}

fn validate_dist_targets(root: &Path, catalog: &Catalog, errors: &mut Vec<String>) {
    #[derive(Deserialize)]
    struct DistWorkspace {
        dist: Option<Dist>,
    }
    #[derive(Deserialize)]
    struct Dist {
        #[serde(default)]
        targets: Vec<String>,
    }

    let path = root.join("dist-workspace.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            errors.push(format!("parse {}: {error}", path.display()));
            return;
        }
    };
    let parsed: DistWorkspace = match toml::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            errors.push(format!("parse {}: {error}", path.display()));
            return;
        }
    };

    let mut actual: Vec<String> = parsed.dist.map(|d| d.targets).unwrap_or_default();
    actual.sort();
    let mut expected: Vec<String> = catalog
        .values()
        .filter(|spec| spec.os == "darwin")
        .map(|spec| spec.rust_target.clone())
        .collect();
    expected.sort();
    assert_set_equal(
        &actual,
        &expected,
        "dist-workspace.toml targets (macOS only; Linux CLI is built by native CI)",
        errors,
    );
}

fn validate_resolve_binary_inline(root: &Path, catalog: &Catalog, errors: &mut Vec<String>) {
    // resolve-binary.js ships inside the published @0xff-ai/omnifs tarball, so
    // it cannot read npm/platforms.json (which sits outside the package). The
    // runtime platform->package map is inlined there; cross-check it so the two
    // cannot drift.
    let path = root.join("npm/omnifs/scripts/resolve-binary.js");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("could not read {}: {error}", path.display()));
            return;
        },
    };
    for spec in catalog.values() {
        let literal = format!("\"{}:{}\": \"{}\"", spec.os, spec.cpu, spec.package);
        if !source.contains(&literal) {
            errors.push(format!(
                "npm/omnifs/scripts/resolve-binary.js missing inline mapping {literal}"
            ));
        }
    }
}

fn platform_dir_entries(dir: &Path) -> Result<Vec<String>> {
    let read_dir = match fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            entries.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(entries)
}

fn single_eq(actual: &Option<Vec<String>>, expected: &str) -> bool {
    matches!(actual, Some(values) if values.len() == 1 && values[0] == expected)
}

fn assert_set_equal(actual: &[String], expected: &[String], label: &str, errors: &mut Vec<String>) {
    let actual_set: BTreeSet<&String> = actual.iter().collect();
    let expected_set: BTreeSet<&String> = expected.iter().collect();
    for value in &actual_set {
        if !expected_set.contains(value) {
            errors.push(format!("{label} has extra entry {value}"));
        }
    }
    for value in &expected_set {
        if !actual_set.contains(value) {
            errors.push(format!("{label} missing entry {value}"));
        }
    }
}

/// Print each error, then a summary line, and exit non-zero. Matches the bun
/// `printErrorsAndExit` so validation output is unchanged for CI consumers.
fn print_errors_and_exit(label: &str, errors: &[String]) -> ! {
    for error in errors {
        eprintln!("error: {error}");
    }
    eprintln!("{label} failed with {} error(s)", errors.len());
    std::process::exit(1);
}
