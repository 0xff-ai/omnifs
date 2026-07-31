//! Declarative resource file path resolution.

use std::path::PathBuf;

pub(crate) fn default_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let path = PathBuf::from("omnifs.k");
    anyhow::ensure!(
        path.is_file(),
        "no omnifs.k in the current directory; pass a path"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::default_path;
    use std::path::PathBuf;

    #[test]
    fn explicit_path_is_preserved() {
        let path = PathBuf::from("some/omnifs.k");
        assert_eq!(default_path(Some(path.clone())).unwrap(), path);
    }
}
