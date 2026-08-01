use crate::source::AuthoringConfig;
use kcl_lang::{API, ExecProgramArgs};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DEFAULT_MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;

/// Result of one KCL evaluation, with no raw KCL runtime objects exposed.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatedConfig {
    pub source: PathBuf,
    pub config: AuthoringConfig,
}

#[derive(Deserialize)]
struct KclOutput {
    config: AuthoringConfig,
}

/// Evaluate KCL on a blocking worker. KCL's API is synchronous, so each call
/// owns its API value inside the worker.
pub async fn evaluate(path: impl Into<PathBuf>) -> Result<EvaluatedConfig, EvaluateError> {
    let path = path.into();
    tokio::task::spawn_blocking(move || evaluate_sync(path))
        .await
        .map_err(EvaluateError::Worker)?
}

/// Synchronous evaluator used by focused tests and blocking callers.
pub(crate) fn evaluate_sync(path: impl Into<PathBuf>) -> Result<EvaluatedConfig, EvaluateError> {
    evaluate_sync_with_limits(path, DEFAULT_MAX_SOURCE_BYTES, DEFAULT_MAX_RESULT_BYTES)
}

fn evaluate_sync_with_limits(
    path: impl Into<PathBuf>,
    max_source_bytes: u64,
    max_result_bytes: usize,
) -> Result<EvaluatedConfig, EvaluateError> {
    let path = path.into();
    reject_url_path(&path)?;
    let source = path.canonicalize().map_err(EvaluateError::Io)?;
    let metadata = std::fs::metadata(&source).map_err(EvaluateError::Io)?;
    if metadata.len() > max_source_bytes {
        return Err(EvaluateError::SourceTooLarge {
            size: metadata.len(),
            max: max_source_bytes,
        });
    }
    let work_dir = source
        .parent()
        .ok_or_else(|| EvaluateError::InvalidSource(source.clone()))?
        .canonicalize()
        .map_err(EvaluateError::Io)?;

    // `external_pkgs` remains empty by construction. KCL resolves only files
    // already present below `work_dir` or its local vendor tree; it has no API
    // switch that disables remote package support, so we never hand it a URL
    // or a package downloader and reject URL-shaped input before this call.
    let args = ExecProgramArgs {
        k_filename_list: vec![source.display().to_string()],
        work_dir: work_dir.display().to_string(),
        ..Default::default()
    };
    let result = API::default()
        .exec_program(&args)
        .map_err(|error| EvaluateError::Kcl(error.to_string()))?;
    if !result.err_message.is_empty() {
        return Err(EvaluateError::Kcl(result.err_message));
    }
    if result.json_result.len() > max_result_bytes {
        return Err(EvaluateError::ResultTooLarge {
            size: result.json_result.len(),
            max: max_result_bytes,
        });
    }
    let KclOutput { config } =
        serde_json::from_str(&result.json_result).map_err(EvaluateError::Authoring)?;
    Ok(EvaluatedConfig { source, config })
}

fn reject_url_path(path: &Path) -> Result<(), EvaluateError> {
    let raw = path.to_string_lossy();
    if raw.contains("://") || raw.starts_with("git:") || raw.starts_with("oci:") {
        return Err(EvaluateError::RemoteSource(raw.into_owned()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum EvaluateError {
    #[error("KCL source path must be local, not `{0}`")]
    RemoteSource(String),
    #[error("KCL source file is invalid: {0}")]
    InvalidSource(PathBuf),
    #[error("KCL source I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("KCL source is {size} bytes, above the {max}-byte limit")]
    SourceTooLarge { size: u64, max: u64 },
    #[error("KCL evaluation failed: {0}")]
    Kcl(String),
    #[error("KCL worker failed: {0}")]
    Worker(#[source] tokio::task::JoinError),
    #[error("KCL JSON output is {size} bytes, above the {max}-byte limit")]
    ResultTooLarge { size: usize, max: usize },
    #[error("KCL config does not match strict omnifs authoring types: {0}")]
    Authoring(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn evaluates_in_process_and_parses_strict_config() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("main.k");
        fs::write(
            &file,
            r#"values = [x * 2 for x in [1, 2]]
config = {apiVersion = "omnifs.dev/v1alpha1", resources = []}"#,
        )
        .unwrap();
        let evaluated = evaluate_sync(&file).unwrap();
        assert_eq!(evaluated.config.resources, Vec::new());
    }

    #[test]
    fn rejects_remote_path_before_kcl() {
        let error = evaluate_sync("https://example.invalid/main.k").unwrap_err();
        assert!(matches!(error, EvaluateError::RemoteSource(_)));
    }

    #[test]
    fn reports_missing_file_and_result_bound() {
        let error = evaluate_sync("missing.k").unwrap_err();
        assert!(matches!(error, EvaluateError::Io(_)));

        let dir = tempdir().unwrap();
        let file = dir.path().join("large.k");
        fs::write(
            &file,
            r#"config = {apiVersion = "omnifs.dev/v1alpha1", resources = [{kind = "Credential", spec = {name = "a", provider = "p", scheme = "oauth", account = "x"}}]}"#,
        )
        .unwrap();
        let error = evaluate_sync_with_limits(&file, DEFAULT_MAX_SOURCE_BYTES, 1).unwrap_err();
        assert!(matches!(error, EvaluateError::ResultTooLarge { .. }));

        let error = evaluate_sync_with_limits(&file, 1, DEFAULT_MAX_RESULT_BYTES).unwrap_err();
        assert!(matches!(error, EvaluateError::SourceTooLarge { .. }));
    }

    #[test]
    fn strict_unknown_field_and_syntax_errors_are_rejected() {
        let dir = tempdir().unwrap();
        let unknown = dir.path().join("unknown.k");
        fs::write(
            &unknown,
            r#"config = {apiVersion = "omnifs.dev/v1alpha1", nope = "x", resources = []}"#,
        )
        .unwrap();
        let unknown_error = evaluate_sync(&unknown).unwrap_err();
        assert!(matches!(unknown_error, EvaluateError::Authoring(_)));
        let syntax = dir.path().join("syntax.k");
        fs::write(&syntax, "config = {").unwrap();
        let error = evaluate_sync(&syntax).unwrap_err();
        assert!(matches!(error, EvaluateError::Kcl(message) if message.contains(":1:")));
    }

    #[test]
    fn local_import_is_allowed_but_missing_package_is_not_downloaded() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("helper.k"), "value = 1").unwrap();
        let file = dir.path().join("main.k");
        fs::write(
            &file,
            r#"import helper
config = {apiVersion = "omnifs.dev/v1alpha1", resources = []}"#,
        )
        .unwrap();
        assert!(evaluate_sync(&file).is_ok());

        let missing = dir.path().join("missing.k");
        fs::write(
            &missing,
            r#"import definitely_missing_remote_package
config = {apiVersion = "omnifs.dev/v1alpha1", resources = []}"#,
        )
        .unwrap();
        let error = evaluate_sync(&missing).unwrap_err();
        assert!(matches!(error, EvaluateError::Kcl(_)));
        assert!(!dir.path().join("vendor").exists());
    }

    #[tokio::test]
    async fn async_evaluation_runs_through_spawn_blocking() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("main.k");
        fs::write(
            &file,
            r#"config = {apiVersion = "omnifs.dev/v1alpha1", resources = []}"#,
        )
        .unwrap();
        let evaluated = evaluate(&file).await.unwrap();
        assert_eq!(evaluated.config.api_version, "omnifs.dev/v1alpha1");
    }
}
