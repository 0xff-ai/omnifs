use omnifs_sdk::prelude::*;
use rc_zip_sync::ReadZip;
use serde::Deserialize;

use crate::http_ext::{GithubHttpExt, github_check_status};
use crate::types::{OwnerName, RepoId, RepoName};
use crate::{Result, State};

#[derive(Clone, Debug, Deserialize)]
struct Run {
    id: u64,
    status: String,
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRunsResponse {
    #[serde(default)]
    workflow_runs: Vec<Run>,
}

pub struct ActionHandlers;

#[handlers]
impl ActionHandlers {
    #[dir("/{owner}/{repo}/_actions/runs")]
    async fn runs(cx: &DirCx<State>, owner: OwnerName, repo: RepoName) -> Result<Projection> {
        let repo_id = RepoId::new(&owner, &repo);
        let runs: WorkflowRunsResponse = cx
            .github_json(format!("/repos/{repo_id}/actions/runs?per_page=30"))
            .await?;

        let mut projection = Projection::new();

        for run in runs.workflow_runs {
            let run_prefix = format!("{repo_id}/_actions/runs/{}", run.id);
            projection.preload(format!("{run_prefix}/status"), run.status);
            projection.preload(
                format!("{run_prefix}/conclusion"),
                run.conclusion.unwrap_or_default(),
            );
            projection.dir(run.id.to_string());
        }
        projection.page(PageStatus::Exhaustive);
        Ok(projection)
    }

    #[dir("/{owner}/{repo}/_actions/runs/{run_id}")]
    async fn run(
        cx: &DirCx<State>,
        owner: OwnerName,
        repo: RepoName,
        run_id: u64,
    ) -> Result<Projection> {
        let repo_id = RepoId::new(&owner, &repo);
        let run: Run = cx
            .github_json(format!("/repos/{repo_id}/actions/runs/{run_id}"))
            .await?;
        let mut projection = Projection::new();
        projection.file_with_content("status", run.status);
        projection.file_with_content("conclusion", run.conclusion.unwrap_or_default());
        if matches!(
            cx.intent(),
            DirIntent::Lookup { .. } | DirIntent::List { .. }
        ) {
            projection.page(PageStatus::Exhaustive);
        }
        Ok(projection)
    }

    #[file("/{owner}/{repo}/_actions/runs/{run_id}/log")]
    async fn run_log(
        cx: &Cx<State>,
        owner: OwnerName,
        repo: RepoName,
        run_id: u64,
    ) -> Result<FileContent> {
        let repo_id = RepoId::new(&owner, &repo);
        let resp = cx
            .github_get(format!("/repos/{repo_id}/actions/runs/{run_id}/logs"))
            .send()
            .await?;
        let body = github_check_status(resp)?.into_body();
        Ok(FileContent::bytes(unzip_logs(&body)))
    }
}

fn unzip_logs(bytes: &[u8]) -> Vec<u8> {
    let Ok(archive) = bytes.read_zip() else {
        return bytes.to_vec();
    };
    let mut output = Vec::new();
    for entry in archive.entries() {
        if entry.name.ends_with('/') {
            continue;
        }
        output.extend_from_slice(format!("=== {} ===\n", entry.name).as_bytes());
        if let Ok(data) = entry.bytes() {
            output.extend_from_slice(&data);
        }
        if !output.ends_with(b"\n") {
            output.push(b'\n');
        }
        if output.len() >= 10 * 1024 * 1024 {
            output.truncate(10 * 1024 * 1024);
            output.extend_from_slice(b"\n[truncated at 10MB]\n");
            return output;
        }
    }
    output
}
