//! Port of upstream `packages/core/src/tool/plugin/write.ts`: create or
//! overwrite a file, creating missing parent directories.
use serde_json::{Value, json};

use ox_schema::wire::ToolDef;
use crate::contract::{Tool, ToolContext, ToolOutput};

use super::diff::{self as Diff, DiffStatus};
use super::{fail, object_param, ok_text, param, required_str, resolve_path, resource_of};

pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn definition(&self) -> ToolDef {
        ToolDef::function(
            "write",
            "Writes a file to the local filesystem, overwriting if one exists.\n\nMissing parent \
             directories are created automatically.\n\nUse this tool to create new files or \
             overwrite existing files. For partial changes, use the edit tool instead.\n\n\
             When to use: add a new file, or intentionally replace a whole file whose full \
             contents you have already decided.\n\
             When NOT to use: change part of an existing file (use edit), rewrite a file you \
             have not read, or append output that belongs in shell.",
            object_param(
                vec![
                    ("path", param("string", "Path to the file to write to")),
                    ("content", param("string", "Content to write to the file")),
                ],
                &["path", "content"],
            ),
        )
    }

    fn execute(
        &self,
        ctx: ToolContext,
        input: Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = ToolOutput> + Send + '_>> {
        Box::pin(async move { run(&ctx, &input) })
    }
}

fn run(ctx: &ToolContext, input: &Value) -> ToolOutput {
    let path = match required_str("write", input, "path") {
        Ok(path) => path,
        Err(output) => return output,
    };
    let content = match required_str("write", input, "content") {
        Ok(content) => content,
        Err(output) => return output,
    };
    let absolute = resolve_path(&ctx.directory, path);
    // Read the previous contents only when the file exists: the diff needs the
    // "before" side, and a created file has none.
    let before = std::fs::read_to_string(&absolute).ok();
    let existed = before.is_some() || absolute.exists();
    // Upstream's description promises automatic parent creation.
    if let Some(parent) = absolute.parent()
        && let Err(_) = std::fs::create_dir_all(parent)
    {
        return fail(format!("Unable to write {path}"));
    }
    if std::fs::write(&absolute, content.as_bytes()).is_err() {
        return fail(format!("Unable to write {path}"));
    }
    let resource = resource_of(&ctx.directory, &absolute);
    let before = before.unwrap_or_default();
    let status = if existed {
        DiffStatus::Modified
    } else {
        DiffStatus::Added
    };
    let diff = Diff::file_diff(&resource, &before, content, status);
    ok_text(format!(
        "{} file successfully: {resource}",
        if existed { "Wrote" } else { "Created" }
    ))
    .with_metadata(json!({ "files": [diff] }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(directory: &tempfile::TempDir) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: directory.path().to_path_buf(),
            call_id: "call_1".to_owned(),
            cancellation: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn creates_new_files_with_parents() {
        let directory = tempdir().unwrap();
        let output = WriteTool
            .execute(
                ctx(&directory),
                json!({"path": "deep/nested/file.txt", "content": "hello\n"}),
            )
            .await;
        assert_eq!(
            output.content,
            json!("Created file successfully: deep/nested/file.txt")
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("deep/nested/file.txt")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_files() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "old").unwrap();
        let output = WriteTool
            .execute(ctx(&directory), json!({"path": "a.txt", "content": "new"}))
            .await;
        assert_eq!(output.content, json!("Wrote file successfully: a.txt"));
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn unwritable_target_fails_with_upstream_message() {
        let directory = tempdir().unwrap();
        // A file used as a directory: parent creation must fail.
        fs::write(directory.path().join("blocker"), "").unwrap();
        let output = WriteTool
            .execute(
                ctx(&directory),
                json!({"path": "blocker/child.txt", "content": "x"}),
            )
            .await;
        assert_eq!(
            output.error.as_deref(),
            Some("Unable to write blocker/child.txt")
        );
    }
}
