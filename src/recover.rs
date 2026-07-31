use anyhow::Result;

use crate::cli::RecoverArgs;
use crate::db::Database;
use crate::groups::{self, ChangeGroup};
use crate::{BOLD, DIM, RESET, find_project, recoveries};

pub fn cmd_recover(args: &RecoverArgs) -> Result<()> {
    if !args.preview && !args.yes {
        anyhow::bail!(
            "No files changed: recovery requires --yes.\nPreview first, then rerun with --yes to give Undo permission to change files."
        );
    }

    // `--before-change` recoveries anchor on a recorded change id instead of
    // a Run: restore the selected files to their state just before that
    // change. This is the CLI twin of the web UI's "undo un-attributed
    // edits" action.
    if let Some(change_id) = args.before_change {
        return recover_before_change(&args.paths, change_id, args.preview, args.yes);
    }

    let run_ref = args
        .run
        .as_deref()
        .expect("clap requires --run unless --before-change is present");
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = db
        .get_run_by_ref(project.id, run_ref)?
        .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", run_ref))?;
    let events = db.get_session_events(&session)?;
    if events.is_empty() {
        println!("Run {} has no recorded file changes.", session.public_id());
        return Ok(());
    }

    let groups = groups::build_groups(&project, &events);
    if groups.is_empty() {
        println!(
            "Run {} has no recoverable groups of file changes.",
            session.public_id()
        );
        return Ok(());
    }

    let (label, paths, kind) = if !args.paths.is_empty() {
        // Exact file selection: "undo these files from the Run, keep the
        // rest". Same shape as the web UI's checkbox-selective undo.
        (
            format!(
                "session '{}' selected files ({})",
                session.name,
                args.paths.len()
            ),
            args.paths.clone(),
            "files",
        )
    } else if let Some(id) = &args.group {
        print_group_summary(&groups);
        let group = find_group(&groups, id)?;
        (
            format!("session '{}' group '{}'", session.name, group.id),
            group.paths.clone(),
            "group",
        )
    } else {
        print_group_summary(&groups);
        (
            format!("session '{}'", session.name),
            groups::all_group_paths(&groups),
            "run",
        )
    };

    let recovery =
        recoveries::create_run_recovery(&session, &paths, &label, kind, "exact-paths", None)?;
    if args.preview {
        return Ok(());
    }
    if args.yes {
        recoveries::cmd_apply(&recovery.public_id())?;
    }
    Ok(())
}

fn recover_before_change(paths: &[String], change_id: i64, preview: bool, yes: bool) -> Result<()> {
    if change_id <= 0 {
        anyhow::bail!(
            "change id must be positive; `undo run show <RUN>` and `undo timeline` list recorded change ids"
        );
    }
    let file_label = if paths.len() == 1 { "file" } else { "files" };
    let recovery = recoveries::create_event_boundary_recovery(
        paths,
        change_id - 1,
        &format!(
            "{} {} to before change {}",
            paths.len(),
            file_label,
            change_id
        ),
        "files",
    )?;
    if preview {
        return Ok(());
    }
    if yes {
        recoveries::cmd_apply(&recovery.public_id())?;
    }
    Ok(())
}

fn find_group<'a>(groups: &'a [ChangeGroup], requested: &str) -> Result<&'a ChangeGroup> {
    let requested = requested.trim().to_ascii_lowercase();
    if requested.is_empty() {
        anyhow::bail!("group id cannot be empty");
    }
    groups
        .iter()
        .find(|group| group.id == requested)
        .ok_or_else(|| {
            let available = groups
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "group '{}' not found. Available groups: {}",
                requested,
                available
            )
        })
}

fn print_group_summary(groups: &[ChangeGroup]) {
    println!("{}File groups in this Run{}", BOLD, RESET);
    for group in groups {
        println!(
            "  {} — {} files, {} recorded changes, +{} -{} {}({}){}",
            group.label,
            group.paths.len(),
            group.event_count,
            group.inserted,
            group.deleted,
            DIM,
            group.id,
            RESET
        );
    }
    println!();
}
