//! /proc/mounts parser.

use std::fs;
use std::path::{Path, PathBuf};

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
    use anyhow::Context;
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(mounts) => mounts,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read /proc/mounts"),
    };
    let wanted = normalize_path(path);
    Ok(parse_proc_mounts(&mounts)
        .into_iter()
        .find(|mount| normalize_path(&mount.mount_point) == wanted))
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}
