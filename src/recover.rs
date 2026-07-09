use anyhow::Result;

use crate::db::Database;
use crate::groups::{self, ChangeGroup};
use crate::{BOLD, DIM, RESET, find_project, restore};

pub fn cmd_recover(
    session_name: &str,
    group_id: Option<&str>,
    preview: bool,
    yes: bool,
) -> Result<()> {
    if !preview && !yes {
        anyhow::bail!("recover writes files. Run with --preview first, then pass --yes to apply.");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = db
        .get_session_by_name(project.id, session_name)?
        .ok_or_else(|| anyhow::anyhow!("session '{}' not found", session_name))?;
    let events = db.get_session_events(&session)?;
    if events.is_empty() {
        println!("No events recorded for session '{}'.", session.name);
        return Ok(());
    }

    let groups = groups::build_groups(&project, &events);
    if groups.is_empty() {
        println!("No change groups found for session '{}'.", session.name);
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

    restore::restore_paths_at_session_start(&paths, &session, &label, preview, yes)
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
    println!("{}Change groups{}", BOLD, RESET);
    for group in groups {
        println!(
            "  {}{}{} {} - {} file(s), {} event(s), +{} -{}",
            DIM,
            group.id,
            RESET,
            group.label,
            group.paths.len(),
            group.event_count,
            group.inserted,
            group.deleted
        );
    }
    println!();
}
