use anyhow::Result;
use chrono::{Local, TimeZone};
use std::path::Path;

use crate::db::Database;
use crate::models::Session;
use crate::{BOLD, DIM, GREEN, RESET, groups, project_for_run, resolve_project};

pub fn cmd_session_start(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("Run name cannot be empty");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;
    let session = db.start_session(project.id, name, "manual")?;

    println!(
        "{}Recording started for Run {}{} ({}) at {}.",
        GREEN,
        session.public_id(),
        RESET,
        session.name,
        format_local_time(session.started_at)
    );
    Ok(())
}

pub fn cmd_session_stop() -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = resolve_project(&db, &cwd)?;

    let Some(session) = stop_session_in(&db, &cwd, project.id)? else {
        println!("No active Run.");
        return Ok(());
    };
    let event_count = db.get_session_events(&session)?.len();
    let change_word = if event_count == 1 {
        "file change"
    } else {
        "file changes"
    };
    println!(
        "{}Run completed{} {} ({}, {} {}).",
        GREEN,
        RESET,
        session.public_id(),
        session.name,
        event_count,
        change_word
    );
    Ok(())
}

/// Synchronize and stop within an explicit context without reading the process cwd.
fn stop_session_in(db: &Database, cwd: &Path, resolved_project_id: i64) -> Result<Option<Session>> {
    let Some(session) = crate::runs::active_run_for_context(db, cwd, resolved_project_id, None)?
    else {
        return Ok(None);
    };
    // The recorder-selected project can change while the Run is active. Its
    // stored project owns both the file history and the completion boundary.
    let project = project_for_run(db, &session)?;
    crate::runs::sync_project(db, &project, Path::new(&project.root_path))?;
    db.complete_run(session.id, "completed").map(Some)
}

pub fn cmd_session_show(name: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let resolved_project = resolve_project(&db, &cwd)?;
    let project_ids = crate::runs::run_context_project_ids(&db, &cwd, resolved_project.id)?;
    let session = crate::runs::find_run_in_context(&db, &project_ids, name)?
        .ok_or_else(|| anyhow::anyhow!("Run '{}' not found", name))?;
    let project = project_for_run(&db, &session)?;
    let events = db.get_session_events(&session)?;
    let groups = groups::build_groups(&project, &events);

    println!(
        "{}Run {}{} — {}",
        BOLD,
        session.public_id(),
        RESET,
        session.name
    );
    println!("Started: {}", format_local_time(session.started_at));
    if let Some(ended_at) = session.ended_at {
        println!("Finished: {}", format_local_time(ended_at));
    }
    println!("File changes: {}", events.len());

    if groups.is_empty() {
        println!();
        println!("No groups of file changes yet.");
        return Ok(());
    }

    println!();
    println!("{}File groups{}", BOLD, RESET);
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
    Ok(())
}

fn format_local_time(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changes already on disk must belong to the Run even if the recorder has
    /// not processed their notifications. Exercise recovery as well as listing.
    #[test]
    fn session_stop_records_pending_file_changes() {
        assert_stop_records_pending_changes(false, false);
    }

    /// Context resolution can switch between parent and nested projects while
    /// a Run is active. Synchronize the Run's project, not the newly resolved one.
    #[test]
    fn session_stop_syncs_run_project_after_topology_changes() {
        assert_stop_records_pending_changes(true, false);
        assert_stop_records_pending_changes(false, true);
    }

    fn assert_stop_records_pending_changes(run_at_parent: bool, resolved_at_parent: bool) {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let parent_root = tree.path().canonicalize().unwrap();
        let nested_root = parent_root.join("nested");
        let cwd = nested_root.join("src");
        std::fs::create_dir_all(&cwd).unwrap();
        let db = Database::open_in_memory().unwrap();
        let parent = db.get_or_create_project(&parent_root).unwrap();
        let nested = db.get_or_create_project(&nested_root).unwrap();
        let project = if run_at_parent { &parent } else { &nested };
        let resolved = if resolved_at_parent { &parent } else { &nested };
        let other = if run_at_parent { &nested } else { &parent };
        let root = Path::new(&project.root_path);
        let modified = root.join("modified.txt");
        let deleted = root.join("deleted.txt");
        let created = root.join("created.txt");
        let kept = root.join("kept.txt");
        std::fs::write(&modified, "original modified").unwrap();
        std::fs::write(&deleted, "original deleted").unwrap();
        std::fs::write(&kept, "keep me").unwrap();
        crate::runs::sync_project(&db, project, root).unwrap();
        let run = db
            .start_session(project.id, "pending-files", "manual")
            .unwrap();

        std::fs::write(&modified, "final modified contents").unwrap();
        std::fs::write(&created, "new contents").unwrap();
        std::fs::remove_file(&deleted).unwrap();
        assert!(db.get_session_events(&run).unwrap().is_empty());

        let completed = stop_session_in(&db, &cwd, resolved.id).unwrap().unwrap();
        assert_eq!(completed.id, run.id);
        assert_eq!(completed.status, "completed");
        let events = db.get_session_events(&completed).unwrap();
        assert_eq!(events.len(), 3);
        for (path, event_type) in [
            (&modified, "MODIFIED"),
            (&deleted, "DELETED"),
            (&created, "CREATED"),
        ] {
            assert!(
                events.iter().any(|event| {
                    event.path == path.to_string_lossy() && event.event_type == event_type
                }),
                "missing {event_type} event for {}",
                path.display()
            );
        }
        assert_eq!(
            completed.end_event_id,
            Some(db.max_event_id(project.id).unwrap())
        );
        assert_eq!(
            db.count_events(other.id).unwrap(),
            0,
            "must not scan another project"
        );

        // A later recorder reconciliation must find these bytes already saved,
        // without duplicating the changes or leaving them outside the Run.
        crate::runs::sync_project(&db, project, root).unwrap();
        assert_eq!(
            completed.end_event_id,
            Some(db.max_event_id(project.id).unwrap())
        );
        assert_eq!(db.get_session_events(&completed).unwrap().len(), 3);
        let paths = [&modified, &deleted, &created]
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let recovery = crate::recoveries::create_run_recovery_in(
            &db,
            project,
            root,
            &completed,
            &paths,
            "pending changes",
            "files",
            "exact-paths",
            None,
        )
        .unwrap();
        let outcome =
            crate::recoveries::apply_recovery_in(&db, project, &recovery.public_id()).unwrap();
        assert_eq!(outcome.files_changed, 3);
        assert_eq!(std::fs::read(&modified).unwrap(), b"original modified");
        assert_eq!(std::fs::read(&deleted).unwrap(), b"original deleted");
        assert!(!created.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), b"keep me");
    }

    /// No active Run remains a successful no-op; ambiguous Runs must fail
    /// before a scan records changes in either project.
    #[test]
    fn session_stop_requires_unique_active_run_before_syncing() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        let nested_root = root.join("nested");
        std::fs::create_dir(&nested_root).unwrap();
        std::fs::write(nested_root.join("pending.txt"), "pending").unwrap();
        let db = Database::open_in_memory().unwrap();
        let parent = db.get_or_create_project(&root).unwrap();
        let nested = db.get_or_create_project(&nested_root).unwrap();
        assert!(
            stop_session_in(&db, &nested_root, nested.id)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.count_events(parent.id).unwrap(), 0);
        assert_eq!(db.count_events(nested.id).unwrap(), 0);

        let parent_run = db.start_session(parent.id, "parent", "manual").unwrap();
        let nested_run = db.start_session(nested.id, "nested", "manual").unwrap();
        let error = stop_session_in(&db, &nested_root, nested.id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("multiple Runs are active"), "{error}");
        for run in [parent_run, nested_run] {
            assert!(db.get_session_by_id(run.id).unwrap().unwrap().is_active());
            assert_eq!(db.count_events(run.project_id).unwrap(), 0);
        }
        assert!(
            !data.path().join("pids").exists(),
            "stop must not start a recorder"
        );
    }

    /// A synchronization error must propagate before the end boundary is saved,
    /// leaving the Run open for a retry after the storage problem is corrected.
    #[test]
    fn session_stop_keeps_run_active_when_sync_fails() {
        let data = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data.path().to_path_buf());
        std::fs::write(data.path().join("snapshots"), "blocks snapshot directory").unwrap();
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::write(root.join("pending.txt"), "pending").unwrap();
        let db = Database::open_in_memory().unwrap();
        let project = db.get_or_create_project(&root).unwrap();
        let run = db
            .start_session(project.id, "sync-failure", "manual")
            .unwrap();

        assert!(stop_session_in(&db, &root, project.id).is_err());
        let unchanged = db.get_session_by_id(run.id).unwrap().unwrap();
        assert!(unchanged.is_active());
        assert!(unchanged.end_event_id.is_none());
        assert_eq!(db.count_events(project.id).unwrap(), 0);
    }
}
