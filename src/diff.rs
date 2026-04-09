use anyhow::Result;
use similar::{ChangeTag, TextDiff};

use crate::db::Database;
use crate::snapshots;
use crate::{find_project, BOLD, DIM, GREEN, RED, RESET};

pub fn cmd_diff(path_str: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;

    let abs_path = cwd.join(path_str);
    let abs_path = if abs_path.exists() {
        abs_path.canonicalize()?
    } else {
        abs_path
    };
    let abs_path_str = abs_path.to_string_lossy().to_string();

    let event = match db.get_latest_event(project.id, &abs_path_str)? {
        Some(e) => e,
        None => {
            println!("No snapshots available for this file.");
            return Ok(());
        }
    };

    if event.event_type == "DELETED" {
        println!(
            "File was deleted. Use {}backtrack restore{} to recover it.",
            BOLD, RESET
        );
        return Ok(());
    }

    let hash = match &event.current_hash {
        Some(h) => h,
        None => {
            println!("No snapshot available for this file.");
            return Ok(());
        }
    };

    let snapshot_content = snapshots::load(project.id, hash)?;
    let snapshot_text = String::from_utf8_lossy(&snapshot_content);

    if !abs_path.exists() {
        println!("File does not exist on disk. Showing last known content.");
        println!();
        for line in snapshot_text.lines() {
            println!(" {}", line);
        }
        return Ok(());
    }

    let current_content = std::fs::read(&abs_path)?;
    let current_text = String::from_utf8_lossy(&current_content);

    if snapshot_text == current_text {
        println!("No changes since last snapshot.");
        return Ok(());
    }

    let rel = crate::relative_path(&abs_path_str, &project.root_path);
    print_unified_diff(&snapshot_text, &current_text, rel);

    Ok(())
}

fn print_unified_diff(old: &str, new: &str, path: &str) {
    let diff = TextDiff::from_lines(old, new);

    println!("{}--- snapshot  {}{}", DIM, path, RESET);
    println!("{}+++ current   {}{}", DIM, path, RESET);
    println!();

    for (idx, group) in diff.grouped_ops(3).iter().enumerate() {
        if idx > 0 {
            println!("{}…{}", DIM, RESET);
        }
        for op in group {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    ChangeTag::Delete => {
                        print!("{}-{}{}", RED, change, RESET);
                    }
                    ChangeTag::Insert => {
                        print!("{}+{}{}", GREEN, change, RESET);
                    }
                    ChangeTag::Equal => {
                        print!(" {}", change);
                    }
                }
                if change.missing_newline() {
                    println!();
                }
            }
        }
    }
}
