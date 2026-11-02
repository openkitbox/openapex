//! Port of upstream `packages/core/src/tool/plugin/write.ts`: create or
//! overwrite a file, creating missing parent directories.
use serde_json::{Value, json};

use crate::contract::{Tool, ToolContext, ToolOutput};
use crate::file_state::Verdict;
use ox_schema::wire::ToolDef;

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
    // Refuse to overwrite a file that changed since this session read it. A new
    // path has no record and passes; a recorded path whose metadata cannot be
    // read now counts as changed, so the guard fails closed.
    if ctx.registry.files.check(&ctx.session_id, &absolute) == Verdict::Changed {
        return fail(format!(
            "Failed to write {path}: the file changed since your last read of it. Read it again before writing."
        ));
    }
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
    ctx.registry.files.record(&ctx.session_id, &absolute);
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::contract::ToolRegistry;

    /// Distinguishes this process's fixtures without reading the clock.
    static FIXTURE: AtomicUsize = AtomicUsize::new(0);

    /// A fixture directory and a context whose registry serves real `read`, so
    /// the stamp a write compares against is the one `read` recorded.
    fn fixture(name: &str) -> (std::path::PathBuf, ToolContext, Arc<ToolRegistry>) {
        let root = std::env::temp_dir().join(format!(
            "ox-write-freshness-{name}-{}-{}",
            std::process::id(),
            FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create the isolated write fixture");
        let registry = Arc::new(ToolRegistry::with_builtins());
        let ctx = ToolContext {
            session_id: "ses_test".to_owned(),
            directory: root.clone(),
            call_id: "call_test".to_owned(),
            cancellation: CancellationToken::new(),
            registry: Arc::clone(&registry),
        };
        (root, ctx, registry)
    }

    #[tokio::test]
    async fn a_write_is_refused_when_the_file_changed_since_the_read() {
        let (root, ctx, registry) = fixture("stale");
        std::fs::write(root.join("note.txt"), "one\n").expect("write the fixture");
        let read = registry.get("read").expect("read is registered");
        let output = read
            .execute(ctx.clone(), json!({ "path": "note.txt" }))
            .await;
        assert_eq!(output.error, None, "the read must succeed before it stales");
        std::fs::write(root.join("note.txt"), "changed out of band\n")
            .expect("rewrite the fixture behind the session's back");

        let output = run(&ctx, &json!({ "path": "note.txt", "content": "mine\n" }));

        assert_eq!(
            output.error.as_deref(),
            Some(
                "Failed to write note.txt: the file changed since your last read of it. \
                 Read it again before writing."
            ),
            "a stale target is refused"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).expect("read the fixture back"),
            "changed out of band\n",
            "the refused write leaves the file untouched"
        );
        std::fs::remove_dir_all(root).expect("remove the isolated write fixture");
    }

    #[tokio::test]
    async fn consecutive_writes_refresh_the_stamp_without_a_reread() {
        let (root, ctx, registry) = fixture("refresh");
        std::fs::write(root.join("note.txt"), "one\n").expect("write the fixture");
        let read = registry.get("read").expect("read is registered");
        let output = read
            .execute(ctx.clone(), json!({ "path": "note.txt" }))
            .await;
        assert_eq!(output.error, None, "the read must succeed");

        let first = run(&ctx, &json!({ "path": "note.txt", "content": "two\n" }));
        let second = run(&ctx, &json!({ "path": "note.txt", "content": "three\n" }));

        assert_eq!(first.error, None, "the first write matches the read stamp");
        assert_eq!(
            second.error, None,
            "a write records the stamp it just produced, so the next write is not stale"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).expect("read the fixture back"),
            "three\n"
        );
        std::fs::remove_dir_all(root).expect("remove the isolated write fixture");
    }

    #[tokio::test]
    async fn a_deleted_path_can_be_written_again() {
        let (root, ctx, registry) = fixture("deleted");
        std::fs::write(root.join("note.txt"), "one\n").expect("write the fixture");
        let read = registry.get("read").expect("read is registered");
        let output = read
            .execute(ctx.clone(), json!({ "path": "note.txt" }))
            .await;
        assert_eq!(output.error, None, "the read must succeed");
        std::fs::remove_file(root.join("note.txt")).expect("delete the fixture out of band");

        let output = run(&ctx, &json!({ "path": "note.txt", "content": "back\n" }));

        assert_eq!(
            output.error, None,
            "a deleted file has nothing to overwrite, so recreating it is allowed"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("note.txt")).expect("read the fixture back"),
            "back\n"
        );
        std::fs::remove_dir_all(root).expect("remove the isolated write fixture");
    }
}
