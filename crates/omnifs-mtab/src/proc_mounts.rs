//! /proc/mounts parser.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: String,
    pub fs_type: String,
}

pub fn parse(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            let mount_point = fields.next()?;
            let fs_type = fields.next()?;
            Some(MountEntry {
                device: decode_mount_field(device),
                mount_point: decode_mount_field(mount_point),
                fs_type: decode_mount_field(fs_type),
            })
        })
        .collect()
}

pub fn find_mount(path: &Path) -> Option<MountEntry> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    parse(&mounts).into_iter().find(|mount| {
        Path::new(&mount.mount_point)
            .components()
            .eq(path.components())
    })
}

fn decode_mount_field(field: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{MountEntry, parse};

    #[test]
    fn decodes_octal_fields() {
        let mounts = parse("127.0.0.1:/omnifs /tmp/omnifs\\040mount nfs4 rw 0 0\n");
        assert_eq!(
            mounts,
            vec![MountEntry {
                device: "127.0.0.1:/omnifs".to_string(),
                mount_point: "/tmp/omnifs mount".to_string(),
                fs_type: "nfs4".to_string(),
            }]
        );
    }
}
