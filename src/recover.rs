use anyhow::Result;

use crate::db::Database;
use crate::groups::{self, ChangeGroup};
use crate::{BOLD, DIM, RESET, find_project, recoveries};

pub fn cmd_recover(
    session_name: &str,
    group_id: Option<&str>,
    preview: bool,
    yes: bool,
) -> Result<()> {
    if !preview && !yes {
        anyhow::bail!(
            "No files changed: recovery requires --yes.\nPreview first, then rerun with --yes to give Undo permission to change files."
        );
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = db
        .get_run_by_ref(project.id, session_name)?
        .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", session_name))?;
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

    print_group_summary(&groups);
    let (label, paths) = match group_id {
        Some(id) => {
            let group = find_group(&groups, id)?;
            (
                format!("session '{}' group '{}'", session.name, group.id),
                group.paths.clone(),
            )
        }
        None => (
            format!("session '{}'", session.name),
            groups::all_group_paths(&groups),
        ),
    };

    let recovery = recoveries::create_run_recovery(
        &session,
        &paths,
        &label,
        if group_id.is_some() { "group" } else { "run" },
        "exact-paths",
        None,
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
