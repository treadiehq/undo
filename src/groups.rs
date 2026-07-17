use crate::models::{FileEvent, WatchedProject};
use crate::{relative_path, snapshots};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub(crate) struct ChangeGroup {
    pub id: String,
    pub label: String,
    pub paths: Vec<String>,
    pub event_count: usize,
    pub inserted: usize,
    pub deleted: usize,
}

pub(crate) fn build_groups(project: &WatchedProject, events: &[FileEvent]) -> Vec<ChangeGroup> {
    let mut grouped: BTreeMap<String, Vec<&FileEvent>> = BTreeMap::new();
    for event in events {
        let rel = relative_path(&event.path, &project.root_path);
        grouped
            .entry(group_key_for_path(rel))
            .or_default()
            .push(event);
    }

    let ids = unique_group_ids(grouped.keys().map(String::as_str));
    grouped
        .into_iter()
        .map(|(key, events)| {
            let id = ids
                .get(&key)
                .expect("every canonical group key has an id")
                .clone();
            group_from_events(id, key, events)
        })
        .collect()
}

pub(crate) fn all_group_paths(groups: &[ChangeGroup]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|group| group.paths.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn group_from_events(id: String, label: String, events: Vec<&FileEvent>) -> ChangeGroup {
    let mut paths = BTreeSet::new();
    let mut inserted = 0usize;
    let mut deleted = 0usize;

    for event in &events {
        paths.insert(event.path.clone());
        if let Some(old_path) = &event.old_path {
            paths.insert(old_path.clone());
        }
        let (adds, dels) = diff_stats_for_event(event);
        inserted += adds;
        deleted += dels;
    }

    ChangeGroup {
        label,
        id,
        paths: paths.into_iter().collect(),
        event_count: events.len(),
        inserted,
        deleted,
    }
}

fn diff_stats_for_event(event: &FileEvent) -> (usize, usize) {
    let old = match event.event_type.as_str() {
        "CREATED" => Vec::new(),
        "DELETED" => event
            .previous_hash
            .as_deref()
            .and_then(|hash| load_snapshot(event.project_id, hash))
            .unwrap_or_default(),
        _ => event
            .previous_hash
            .as_deref()
            .and_then(|hash| load_snapshot(event.project_id, hash))
            .unwrap_or_default(),
    };
    let new = match event.event_type.as_str() {
        "DELETED" => Vec::new(),
        _ => event
            .current_hash
            .as_deref()
            .and_then(|hash| load_snapshot(event.project_id, hash))
            .unwrap_or_default(),
    };

    if crate::diff::is_binary(&old) || crate::diff::is_binary(&new) {
        return (0, 0);
    }

    if old == new {
        return (0, 0);
    }
    let (old_text, new_text) = crate::diff::render_bytes_for_diff(&old, &new);
    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Insert => inserted += 1,
            ChangeTag::Equal => {}
        }
    }
    (inserted, deleted)
}

fn load_snapshot(project_id: i64, hash: &str) -> Option<Vec<u8>> {
    // Grouping is a summary aid; missing/pruned snapshots should not block listing
    // the session or previewing a path-level restore plan.
    snapshots::load(project_id, hash).ok()
}

fn group_key_for_path(rel_path: &str) -> String {
    let mut parts = rel_path.split('/').filter(|part| !part.is_empty());
    let first = parts.next().unwrap_or("root");
    match first {
        "src" | "app" | "lib" | "crates" | "packages" | "components" => parts
            .next()
            .map(|part| format!("{first}/{part}"))
            .unwrap_or_else(|| first.to_string()),
        other => other.to_string(),
    }
}

fn preferred_group_id(key: &str) -> String {
    let candidate = key.strip_prefix("src/").unwrap_or(key);
    slugify(path_stem(candidate))
}

fn unique_group_ids<'a>(keys: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    let keys = keys.map(str::to_string).collect::<Vec<_>>();
    let mut base_counts = BTreeMap::<String, usize>::new();
    for key in &keys {
        *base_counts.entry(preferred_group_id(key)).or_default() += 1;
    }

    let mut ids = BTreeMap::new();
    let mut used = BTreeSet::new();
    for key in keys {
        let base = preferred_group_id(&key);
        let mut id = if base_counts[&base] == 1 {
            base.clone()
        } else {
            format!("{}-{}", base, group_key_hash(&key, 4))
        };
        if used.contains(&id) {
            id = format!("{}-{}", base, group_key_hash(&key, 32));
        }
        let mut collision = 2usize;
        while !used.insert(id.clone()) {
            id = format!("{}-{}-{}", base, group_key_hash(&key, 32), collision);
            collision += 1;
        }
        ids.insert(key, id);
    }
    ids
}

fn group_key_hash(key: &str, bytes: usize) -> String {
    let digest = Sha256::digest(key.as_bytes());
    crate::to_hex(&digest[..bytes])
}

fn path_stem(path: &str) -> &str {
    path.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(path)
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "root".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> WatchedProject {
        WatchedProject {
            id: 1,
            root_path: "/repo".to_string(),
            created_at: 0,
        }
    }

    fn event(id: i64, path: &str) -> FileEvent {
        FileEvent {
            id,
            project_id: 1,
            timestamp: 100 + id,
            path: path.to_string(),
            event_type: "MODIFIED".to_string(),
            current_hash: None,
            previous_hash: None,
            snapshot_path: None,
            old_path: None,
            file_size: None,
        }
    }

    #[test]
    fn groups_events_by_module_path() {
        let groups = build_groups(
            &project(),
            &[
                event(1, "/repo/src/auth/login.rs"),
                event(2, "/repo/src/auth/session.rs"),
                event(3, "/repo/src/billing/invoice.rs"),
            ],
        );

        let ids = groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["auth", "billing"]);
        assert_eq!(groups[0].label, "src/auth");
        assert_eq!(groups[1].label, "src/billing");
        assert_eq!(groups[0].paths.len(), 2);
    }

    #[test]
    fn group_ids_are_stable_for_root_files() {
        let groups = build_groups(
            &project(),
            &[event(1, "/repo/Cargo.toml"), event(2, "/repo/README.md")],
        );

        let ids = groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["cargo", "readme"]);
        let labels = groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["Cargo.toml", "README.md"]);
    }

    #[test]
    fn special_prefixes_with_the_same_module_name_are_separate_groups() {
        let groups = build_groups(
            &project(),
            &[
                event(1, "/repo/app/auth/login.rs"),
                event(2, "/repo/lib/auth/token.rs"),
            ],
        );

        let ids = groups
            .iter()
            .map(|group| group.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["app-auth", "lib-auth"]);
        let labels = groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["app/auth", "lib/auth"]);
        assert!(groups.iter().all(|group| group.paths.len() == 1));
    }

    #[test]
    fn colliding_slugs_get_distinct_stable_ids_without_merging() {
        let events = [
            event(1, "/repo/app/auth/login.rs"),
            event(2, "/repo/app-auth/token.rs"),
        ];
        let groups = build_groups(&project(), &events);
        let rebuilt = build_groups(&project(), &events);

        assert_eq!(groups.len(), 2);
        assert_ne!(groups[0].id, groups[1].id);
        assert!(groups.iter().all(|group| group.id.starts_with("app-auth-")));
        assert!(groups.iter().all(|group| group.paths.len() == 1));
        assert_eq!(
            groups
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>(),
            rebuilt
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn group_stats_count_different_invalid_utf8_bytes() {
        let data_dir = tempfile::tempdir().unwrap();
        crate::set_test_data_dir(data_dir.path().to_path_buf());
        let old = b"\xe9l\xe8ve\n";
        let new = b"\xe9l\xe9ve\n";
        let old_hash = snapshots::hash_bytes(old);
        let new_hash = snapshots::hash_bytes(new);
        let publish_guard = snapshots::acquire_publish_guard().unwrap();
        snapshots::save_durable(&publish_guard, 1, &old_hash, old).unwrap();
        snapshots::save_durable(&publish_guard, 1, &new_hash, new).unwrap();
        let mut changed = event(1, "/repo/src/auth/login.txt");
        changed.previous_hash = Some(old_hash);
        changed.current_hash = Some(new_hash);

        assert_eq!(diff_stats_for_event(&changed), (1, 1));
    }

    /// Reverting a rename requires both names: write the source path as it
    /// existed before the session and remove the destination created by it.
    #[test]
    fn renamed_events_include_old_path_in_group_paths() {
        let mut renamed = event(1, "/repo/src/billing/signin.rs");
        renamed.event_type = "RENAMED".to_string();
        renamed.old_path = Some("/repo/src/auth/login.rs".to_string());

        let groups = build_groups(&project(), &[renamed]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "billing");
        assert_eq!(
            groups[0].paths,
            vec![
                "/repo/src/auth/login.rs".to_string(),
                "/repo/src/billing/signin.rs".to_string()
            ]
        );
    }
}
