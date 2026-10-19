//! Shared stub `codebase-memory-mcp` binaries for the gateway and tool tests.
//!
//! The scripts write their observations next to the stub's working directory,
//! so each test simply inspects the temp dir it passed as `cwd`.

use std::path::Path;

use serde_json::Value;

use super::Cbm;

/// Records argv and stdin, logs the spawned subcommand, serves
/// `list_projects` as JSON only when `--format json` is present, and fails
/// when the arguments carry `"cbm_fail"`.
pub(crate) const MAIN_STUB: &str = r#"#!/bin/sh
# $1=cli $2=--quiet $3=<tool>
printf '%s\n' "$@" > .cbm-stub-argv
printf '%s\n' "$3" >> .cbm-stub-spawns
tool="$3"
cat > .cbm-stub-stdin.json
case "$tool" in
  list_projects)
    case " $* " in
      *" --format json "*) cat .cbm-stub-projects.json 2>/dev/null || printf '{"projects":[]}' ;;
      *) printf 'tree-output\n' ;;
    esac
    ;;
  *)
    if grep -q '"cbm_fail"' .cbm-stub-stdin.json 2>/dev/null; then
      printf '{"error":"stub boom","hint":"fix it"}' >&2
      exit 1
    fi
    printf 'stub %s ok\n' "$tool"
    ;;
esac
"#;

/// Announces itself, then sleeps so cancellation must kill it.
pub(crate) const SLOW_STUB: &str = r#"#!/bin/sh
cat > .cbm-stub-stdin.json
printf started > .cbm-stub-started
sleep 5
printf 'late\n'
"#;

/// Exits non-zero without writing a diagnostic.
pub(crate) const SILENT_STUB: &str = r#"#!/bin/sh
cat > .cbm-stub-stdin.json
exit 1
"#;

/// Emits a prepared stderr payload and fails.
pub(crate) const STDERR_STUB: &str = r#"#!/bin/sh
cat > .cbm-stub-stdin.json
cat .cbm-stub-stderr.txt >&2
exit 1
"#;

/// Install `script` at `path` and build a [`Cbm`] pointing at it.
pub(crate) fn stub(path: &Path, script: &str) -> Cbm {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    Cbm::new(path.to_path_buf())
}

/// Write the `list_projects --format json` response the stub serves.
pub(crate) fn write_projects(directory: &Path, projects: Value) {
    std::fs::write(
        directory.join(".cbm-stub-projects.json"),
        serde_json::to_string(&projects).unwrap(),
    )
    .unwrap();
}

/// The subcommands the stub was spawned for, in spawn order.
pub(crate) fn spawns(directory: &Path) -> Vec<String> {
    std::fs::read_to_string(directory.join(".cbm-stub-spawns"))
        .map(|log| log.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The most recent JSON object the stub read from stdin.
pub(crate) fn stdin_json(directory: &Path) -> Value {
    let raw = std::fs::read_to_string(directory.join(".cbm-stub-stdin.json"))
        .expect("stub captured stdin");
    serde_json::from_str(&raw).expect("stub stdin is JSON")
}
