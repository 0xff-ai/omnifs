//! /proc/mounts parser and running-mount argument inference.

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

pub(crate) fn parse_mount_command_args(args: &[String]) -> Option<RunningMountArgs> {
    let executable = Path::new(args.first()?).file_name()?.to_str()?;
    if executable != "omnifs" {
        return None;
    }
    if args.get(1).map(String::as_str) != Some("daemon") {
        return None;
    }
    if args.get(2).map(String::as_str) != Some("mount") {
        return None;
    }

    let mut parsed = RunningMountArgs::default();
    let mut idx = 3; // skip [omnifs, daemon, mount]
    while idx < args.len() {
        let (key, inline_value) = args[idx].split_once('=').map_or_else(
            || (args[idx].as_str(), None),
            |(key, value)| (key, Some(value)),
        );

        if matches!(key, "--mount-point" | "--config-dir" | "--cache-dir") {
            let uses_inline_value = inline_value.is_some();
            let value = inline_value
                .map(PathBuf::from)
                .or_else(|| args.get(idx + 1).map(PathBuf::from));
            match key {
                "--mount-point" => parsed.mount_point = value,
                "--config-dir" => parsed.config_dir = value,
                "--cache-dir" => parsed.cache_dir = value,
                _ => {},
            }
            idx += if uses_inline_value { 1 } else { 2 };
            continue;
        }

        idx += 1;
    }

    Some(parsed)
}

pub(crate) fn infer_running_mount_args() -> Option<RunningMountArgs> {
    let proc_dir = Path::new("/proc");
    let entries = fs::read_dir(proc_dir).ok()?;

    for entry in entries.filter_map(Result::ok) {
        let file_name = entry.file_name();
        if file_name.to_string_lossy().parse::<u32>().is_err() {
            continue;
        }

        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }

        let args = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();

        if let Some(parsed) = parse_mount_command_args(&args) {
            return Some(parsed);
        }
    }

    None
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
