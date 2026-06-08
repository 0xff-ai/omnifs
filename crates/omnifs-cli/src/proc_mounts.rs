//! /proc/mounts parser.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountInfo {
    pub(crate) source: String,
    pub(crate) mount_point: PathBuf,
    pub(crate) fs_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RunningMountArgs {
    pub(crate) mount_point: Option<PathBuf>,
    pub(crate) config_dir: Option<PathBuf>,
    pub(crate) cache_dir: Option<PathBuf>,
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn decode_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let octal = &field[i + 1..i + 4];
            if let Ok(value) = u8::from_str_radix(octal, 8) {
                out.push(char::from(value));
                i += 4;
                continue;
            }
        }

        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_mounts(contents: &str) -> Vec<MountInfo> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let mount_point = fields.next()?;
            let fs_type = fields.next()?;
            Some(MountInfo {
                source: decode_mount_field(source),
                mount_point: PathBuf::from(decode_mount_field(mount_point)),
                fs_type: decode_mount_field(fs_type),
            })
        })
        .collect()
}

pub(crate) fn find_mount(path: &Path) -> anyhow::Result<Option<MountInfo>> {
    let mounts = mount_table()?;
    let wanted = normalize_path(path);
    let canonical = normalize_path(&canonical_mount_path(path));
    Ok(mounts.into_iter().find(|mount| {
        let mounted = normalize_path(&mount.mount_point);
        mounted == wanted || mounted == canonical
    }))
}

#[cfg(target_os = "linux")]
fn mount_table() -> anyhow::Result<Vec<MountInfo>> {
    use anyhow::Context;
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(mounts) => mounts,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("failed to read /proc/mounts"),
    };
    Ok(parse_proc_mounts(&mounts))
}

#[cfg(target_os = "macos")]
fn mount_table() -> anyhow::Result<Vec<MountInfo>> {
    use anyhow::Context;
    let output = Command::new("mount")
        .output()
        .context("failed to run mount")?;
    if !output.status.success() {
        anyhow::bail!("mount exited with {}", output.status);
    }
    Ok(parse_macos_mounts(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mount_table() -> anyhow::Result<Vec<MountInfo>> {
    Ok(Vec::new())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_macos_mounts(contents: &str) -> Vec<MountInfo> {
    contents
        .lines()
        .filter_map(|line| {
            let (mount, options) = line.rsplit_once(" (")?;
            let (source, mount_point) = mount.rsplit_once(" on ")?;
            let fs_type = options
                .split_once(',')
                .map_or_else(|| options.trim_end_matches(')'), |(fs_type, _)| fs_type);
            Some(MountInfo {
                source: source.to_string(),
                mount_point: PathBuf::from(mount_point),
                fs_type: fs_type.to_string(),
            })
        })
        .collect()
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

fn canonical_mount_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_macos_mounts_extracts_nfs_mounts() {
        let mounts = parse_macos_mounts(
            "127.0.0.1:/omnifs on /private/var/folders/x/mnt (nfs, read-only)\n",
        );

        assert_eq!(
            mounts,
            vec![MountInfo {
                source: "127.0.0.1:/omnifs".to_string(),
                mount_point: PathBuf::from("/private/var/folders/x/mnt"),
                fs_type: "nfs".to_string(),
            }]
        );
    }

    #[test]
    fn parse_proc_mounts_decodes_octal_escapes() {
        let mounts = parse_proc_mounts("/dev/disk /path/with\\040space ext4 rw 0 0\n");

        assert_eq!(
            mounts,
            vec![MountInfo {
                source: "/dev/disk".to_string(),
                mount_point: PathBuf::from("/path/with space"),
                fs_type: "ext4".to_string(),
            }]
        );
    }
}
