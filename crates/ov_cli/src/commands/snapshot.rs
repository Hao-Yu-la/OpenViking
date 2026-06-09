use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

use crate::client::{HttpClient, SnapshotCommitReq, SnapshotRestoreReq, SnapshotShowResult};
use crate::error::Result;
use crate::output::{OutputFormat, output_success};
use crate::SnapshotCmd;

pub async fn dispatch(
    client: &HttpClient,
    cmd: SnapshotCmd,
    output_format: OutputFormat,
    compact: bool,
) -> Result<()> {
    match cmd {
        SnapshotCmd::Commit {
            message,
            paths,
            branch,
            author_name,
            author_email,
        } => {
            let req = SnapshotCommitReq {
                message,
                paths,
                branch,
                author_name,
                author_email,
            };
            let value = client.snapshot_commit(&req).await?;
            print_commit(&value, output_format, compact);
            Ok(())
        }
        SnapshotCmd::Restore {
            project_dir,
            source_commit,
            branch,
            dry_run,
            message,
            author_name,
            author_email,
        } => {
            let req = SnapshotRestoreReq {
                project_dir,
                source_commit,
                branch,
                dry_run,
                message,
                author_name,
                author_email,
            };
            let value = client.snapshot_restore(&req).await?;
            print_restore(&value, output_format, compact);
            Ok(())
        }
        SnapshotCmd::Show {
            target_ref,
            path,
            out_path,
        } => {
            let result = client.snapshot_show(&target_ref, path.as_deref()).await?;
            handle_show(result, out_path, output_format, compact)
        }
        SnapshotCmd::Log { branch, limit } => {
            let value = client.snapshot_log(&branch, limit).await?;
            print_log(&value, output_format, compact);
            Ok(())
        }
    }
}

fn print_commit(value: &Value, output_format: OutputFormat, compact: bool) {
    if matches!(output_format, OutputFormat::Json) {
        output_success(value, output_format, compact);
        return;
    }
    // The server returns the inner result dict (BaseClient unwraps the envelope).
    // value is already the "result" field from {status, result}.
    let result_kind = value.get("result").and_then(|v| v.as_str()).unwrap_or("");
    let oid = value.get("commit_oid").and_then(|v| v.as_str()).unwrap_or("");
    match result_kind {
        "created" => {
            let changed = value.get("changed").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("Created {} ({} files changed)", oid, changed);
        }
        "noop" => println!("No changes — nothing to commit"),
        _ => println!("{}", oid),
    }
}

fn print_restore(value: &Value, output_format: OutputFormat, compact: bool) {
    if matches!(output_format, OutputFormat::Json) {
        output_success(value, output_format, compact);
        return;
    }
    // Dry-run shape: {diff: {to_write, to_delete, unchanged}, head, source}
    // Applied shape: {result: "applied", new_commit_oid, source_commit, ...}
    // Noop shape: {result: "noop", head, source}
    if let Some(diff) = value.get("diff") {
        println!("Dry-run:");
        let to_write = diff
            .get("to_write")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let to_delete = diff
            .get("to_delete")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let unchanged = diff
            .get("unchanged")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        println!("  to_write:  {}", to_write);
        println!("  to_delete: {}", to_delete);
        println!("  unchanged: {}", unchanged);
        return;
    }
    let kind = value.get("result").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "applied" => {
            let new_short = value
                .get("new_commit_oid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .get(..12)
                .unwrap_or("");
            let src_short = value
                .get("source_commit")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .get(..12)
                .unwrap_or("");
            let written = value.get("written").and_then(|v| v.as_u64()).unwrap_or(0);
            let deleted = value.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0);
            let unchanged = value.get("unchanged").and_then(|v| v.as_u64()).unwrap_or(0);
            println!(
                "Restored from {} as {} ({} written, {} deleted, {} unchanged)",
                src_short, new_short, written, deleted, unchanged
            );
        }
        "noop" => println!("Already at source — no changes"),
        _ => output_success(value, OutputFormat::Json, compact),
    }
}

fn handle_show(
    result: SnapshotShowResult,
    out_path: Option<PathBuf>,
    output_format: OutputFormat,
    compact: bool,
) -> Result<()> {
    match result {
        SnapshotShowResult::Metadata(meta) => {
            if matches!(output_format, OutputFormat::Json) {
                output_success(&meta, output_format, compact);
                return Ok(());
            }
            for key in &["oid", "tree", "author", "committer"] {
                if let Some(v) = meta.get(*key) {
                    println!("{}: {}", key, v);
                }
            }
            if let Some(parents) = meta.get("parents").and_then(|v| v.as_array()) {
                let names: Vec<String> = parents
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                println!("parents: [{}]", names.join(", "));
            }
            if let Some(msg) = meta.get("message").and_then(|v| v.as_str()) {
                println!();
                println!("{}", msg);
            }
            Ok(())
        }
        SnapshotShowResult::Blob { oid, bytes, size } => {
            if matches!(output_format, OutputFormat::Json) {
                let envelope = serde_json::json!({"oid": oid, "size": size});
                output_success(&envelope, output_format, compact);
                if let Some(path) = out_path {
                    let mut f = std::fs::File::create(&path)?;
                    f.write_all(&bytes)?;
                }
                return Ok(());
            }
            match out_path {
                Some(path) => {
                    let mut f = std::fs::File::create(&path)?;
                    f.write_all(&bytes)?;
                    eprintln!("Wrote {} bytes from {} to {}", size, &oid[..12.min(oid.len())], path.display());
                }
                None => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&bytes)?;
                    eprintln!("Read {} bytes from {}", size, &oid[..12.min(oid.len())]);
                }
            }
            Ok(())
        }
    }
}

fn print_log(value: &Value, output_format: OutputFormat, compact: bool) {
    if matches!(output_format, OutputFormat::Json) {
        output_success(value, output_format, compact);
        return;
    }
    // value is the unwrapped "result" — a JSON array of commit entries.
    let entries = value
        .as_array()
        .cloned()
        .unwrap_or_default();

    for entry in entries {
        let oid = entry.get("oid").and_then(|v| v.as_str()).unwrap_or("");
        let short = oid.get(..12).unwrap_or(oid);
        let msg_full = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let subject = msg_full.lines().next().unwrap_or("");
        let author = entry
            .get("author")
            .and_then(|a| a.get("name").or_else(|| a.as_str().map(|_| a)))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!("{}  {:20}  {}", short, author, subject);
    }
}
