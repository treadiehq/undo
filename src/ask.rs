use anyhow::Result;
use std::collections::BTreeSet;

use crate::db::Database;
use crate::groups::{self, ChangeGroup};
use crate::models::{Session, WatchedProject};
use crate::{BOLD, DIM, GREEN, RESET, YELLOW, find_project, restore};

#[derive(Debug)]
struct AskIntent {
    revert_terms: Vec<String>,
    keep_terms: Vec<String>,
    revert_all: bool,
}

#[derive(Debug)]
struct AskProposal {
    session: Session,
    revert_groups: Vec<ChangeGroup>,
    keep_groups: Vec<ChangeGroup>,
    unmatched_terms: Vec<String>,
}

pub fn cmd_ask(query: &str, session_name: Option<&str>, apply: bool, yes: bool) -> Result<()> {
    if apply && !yes {
        anyhow::bail!("ask --apply requires --yes");
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let project = find_project(&db, &cwd)?;
    let session = resolve_session(&db, project.id, session_name)?;
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

    let proposal = build_proposal(query, session, &project, &groups)?;
    print_proposal(query, &proposal, apply);

    if proposal.revert_groups.is_empty() {
        println!();
        println!(
            "No matching rollback target found. Run `undo session show {}` to inspect group ids.",
            proposal.session.name
        );
        return Ok(());
    }

    if !apply {
        println!();
        println!(
            "Apply command: undo ask {:?} --session {} --apply --yes",
            query, proposal.session.name
        );
        return Ok(());
    }

    let paths = groups::all_group_paths(&proposal.revert_groups);
    let label = format!("ask proposal for session '{}'", proposal.session.name);
    restore::restore_paths_at_session_start(&paths, &proposal.session, &label, false, yes)
}

fn resolve_session(db: &Database, project_id: i64, name: Option<&str>) -> Result<Session> {
    if let Some(name) = name {
        return db
            .get_session_by_name(project_id, name)?
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name));
    }

    db.list_sessions(project_id)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no sessions found. Run `undo session start <name>` first."))
}

fn build_proposal(
    query: &str,
    session: Session,
    project: &WatchedProject,
    groups: &[ChangeGroup],
) -> Result<AskProposal> {
    let intent = parse_intent(query);
    let keep_groups = select_groups(groups, project, &intent.keep_terms);
    let keep_ids = keep_groups
        .iter()
        .map(|group| group.id.as_str())
        .collect::<BTreeSet<_>>();

    let mut revert_groups = if intent.revert_all {
        groups.to_vec()
    } else {
        select_groups(groups, project, &intent.revert_terms)
    };
    revert_groups.retain(|group| !keep_ids.contains(group.id.as_str()));

    let matched_terms = matched_terms(groups, project, &intent);
    let requested_terms = intent
        .revert_terms
        .iter()
        .chain(intent.keep_terms.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let unmatched_terms = requested_terms
        .difference(&matched_terms)
        .cloned()
        .collect::<Vec<_>>();

    Ok(AskProposal {
        session,
        revert_groups,
        keep_groups,
        unmatched_terms,
    })
}

fn parse_intent(query: &str) -> AskIntent {
    let (revert_part, keep_part) = split_keep_clause(query);
    let revert_all = contains_all_intent(&normalize(revert_part));
    AskIntent {
        revert_terms: important_terms(revert_part),
        keep_terms: important_terms(keep_part.unwrap_or_default()),
        revert_all,
    }
}

fn split_keep_clause(query: &str) -> (&str, Option<&str>) {
    let lowercase = query.to_ascii_lowercase();
    for marker in [" but keep ", " except ", " keep ", " without "] {
        if let Some(marker_start) = lowercase.find(marker) {
            let after_start = marker_start + marker.len();
            return (&query[..marker_start], Some(&query[after_start..]));
        }
    }
    (query, None)
}

fn select_groups(
    groups: &[ChangeGroup],
    project: &WatchedProject,
    terms: &[String],
) -> Vec<ChangeGroup> {
    let mut scored = groups
        .iter()
        .filter_map(|group| {
            let score = score_group(group, project, terms);
            (score > 0).then(|| (score, group.clone()))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    scored.into_iter().map(|(_, group)| group).collect()
}

fn matched_terms(
    groups: &[ChangeGroup],
    project: &WatchedProject,
    intent: &AskIntent,
) -> BTreeSet<String> {
    intent
        .revert_terms
        .iter()
        .chain(intent.keep_terms.iter())
        .filter(|term| {
            groups
                .iter()
                .any(|group| score_group(group, project, &[*term]) > 0)
        })
        .cloned()
        .collect()
}

fn score_group(group: &ChangeGroup, project: &WatchedProject, terms: &[impl AsRef<str>]) -> usize {
    let mut score = 0usize;
    let id = group.id.to_ascii_lowercase();
    let compact_id = compact(&id);
    let label = group.label.to_ascii_lowercase();
    for term in terms {
        let term = term.as_ref();
        if term.is_empty() {
            continue;
        }
        if id == term {
            score += 10;
        } else if compact_id == compact(term) {
            score += 8;
        } else if id.contains(term) || term.contains(&id) {
            score += 6;
        }
        if label.split_whitespace().any(|part| part == term) {
            score += 5;
        } else if label.contains(term) {
            score += 3;
        }
        for path in &group.paths {
            let rel = crate::relative_path(path, &project.root_path).to_ascii_lowercase();
            if rel
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|part| part == term)
            {
                score += 2;
            } else if rel.contains(term) {
                score += 1;
            }
        }
    }
    score
}

fn compact(input: &str) -> String {
    input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn print_proposal(query: &str, proposal: &AskProposal, apply: bool) {
    println!("{}undo ask{} — rollback proposal", BOLD, RESET);
    println!("Query:   {}", query);
    println!("Session: {}", proposal.session.name);
    println!();

    if proposal.revert_groups.is_empty() {
        println!("{}Would revert{} nothing yet.", YELLOW, RESET);
    } else {
        println!("{}Would revert{}", YELLOW, RESET);
        for group in &proposal.revert_groups {
            print_group(group);
        }
    }

    if !proposal.keep_groups.is_empty() {
        println!();
        println!("{}Would keep{}", GREEN, RESET);
        for group in &proposal.keep_groups {
            print_group(group);
        }
    }

    if !proposal.unmatched_terms.is_empty() {
        println!();
        println!(
            "{}Unmatched terms{}: {}",
            DIM,
            RESET,
            proposal.unmatched_terms.join(", ")
        );
    }

    println!();
    if apply {
        println!("Applying proposal because --apply --yes was provided.");
    } else {
        println!("No files changed. Add --apply --yes to apply this proposal.");
    }
}

fn print_group(group: &ChangeGroup) {
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

fn contains_all_intent(input: &str) -> bool {
    input
        .split_whitespace()
        .any(|word| matches!(word, "all" | "everything" | "session"))
}

fn important_terms(input: &str) -> Vec<String> {
    normalize(input)
        .split_whitespace()
        .filter(|word| !is_stopword(word))
        .map(stem)
        .filter(|word| word.len() > 1)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn stem(word: &str) -> String {
    for suffix in ["ing", "ments", "ment", " fixes", "fixes", "s"] {
        if word.len() > suffix.len() + 2 && word.ends_with(suffix) {
            return word.trim_end_matches(suffix).to_string();
        }
    }
    word.to_string()
}

fn is_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "and"
            | "the"
            | "this"
            | "that"
            | "these"
            | "those"
            | "undo"
            | "revert"
            | "rollback"
            | "roll"
            | "back"
            | "go"
            | "before"
            | "from"
            | "last"
            | "week"
            | "but"
            | "keep"
            | "except"
            | "without"
            | "agent"
            | "did"
            | "in"
            | "of"
            | "to"
            | "for"
            | "with"
            | "new"
            | "change"
            | "changes"
            | "improvement"
            | "improvements"
            | "refactor"
            | "layer"
            | "session"
    )
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

    fn session() -> Session {
        Session {
            id: 1,
            project_id: 1,
            name: "agent-auth-work".to_string(),
            kind: "manual".to_string(),
            started_at: 100,
            ended_at: Some(200),
            start_event_id: 10,
            end_event_id: Some(20),
            created_at: 100,
        }
    }

    fn group(id: &str, paths: &[&str]) -> ChangeGroup {
        ChangeGroup {
            id: id.to_string(),
            label: id
                .split('-')
                .map(|part| {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            event_count: paths.len(),
            inserted: 0,
            deleted: 0,
        }
    }

    #[test]
    fn intent_splits_revert_and_keep_terms() {
        let intent = parse_intent("undo the auth refactor but keep the security improvements");

        assert!(intent.revert_terms.contains(&"auth".to_string()));
        assert!(intent.keep_terms.contains(&"security".to_string()));
        assert!(!intent.revert_all);
    }

    /// Clause keywords inside hyphenated group ids are target words, not
    /// instructions to preserve the rest of the query.
    #[test]
    fn intent_does_not_split_hyphenated_group_ids() {
        for (query, expected_term) in [
            ("revert keep-alive", "alive"),
            ("revert except-handler", "handler"),
            ("revert without-cache", "cache"),
        ] {
            let intent = parse_intent(query);
            assert!(
                intent.keep_terms.is_empty(),
                "{query:?} unexpectedly produced keep terms"
            );
            assert!(
                intent.revert_terms.contains(&expected_term.to_string()),
                "{query:?} did not preserve its target term"
            );
        }
    }

    /// Splitting the raw query must remain case-insensitive and retain the
    /// concise comma-plus-keep syntax used in the CLI documentation.
    #[test]
    fn intent_splits_bare_keep_clause_case_insensitively() {
        let intent = parse_intent("undo auth, KEEP security");

        assert_eq!(intent.revert_terms, vec!["auth"]);
        assert_eq!(intent.keep_terms, vec!["security"]);
    }

    #[test]
    fn proposal_reverts_matching_group_and_keeps_keep_group() {
        let groups = vec![
            group("auth", &["/repo/src/auth/login.rs"]),
            group("security", &["/repo/src/security/csrf.rs"]),
        ];

        let proposal = build_proposal(
            "undo the auth refactor but keep the security improvements",
            session(),
            &project(),
            &groups,
        )
        .unwrap();

        assert_eq!(
            proposal
                .revert_groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auth"]
        );
        assert_eq!(
            proposal
                .keep_groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["security"]
        );
    }

    /// Group ids should match whether users include or omit generated
    /// punctuation such as the dash in `keep-alive`.
    #[test]
    fn proposal_matches_compact_group_id() {
        let groups = vec![group("keep-alive", &["/repo/src/keep-alive/ping.rs"])];

        for query in ["revert keep-alive", "revert keepalive"] {
            let proposal = build_proposal(query, session(), &project(), &groups).unwrap();

            assert_eq!(
                proposal
                    .revert_groups
                    .iter()
                    .map(|group| group.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["keep-alive"],
                "{query:?} did not select the requested group"
            );
            assert!(proposal.keep_groups.is_empty());
        }
    }

    #[test]
    fn everything_except_keeps_matching_group() {
        let groups = vec![
            group("auth", &["/repo/src/auth/login.rs"]),
            group("bug-fixes", &["/repo/src/fixes/null.rs"]),
        ];

        let proposal = build_proposal(
            "revert everything the agent did in the last session except the bug fixes",
            session(),
            &project(),
            &groups,
        )
        .unwrap();

        assert_eq!(
            proposal
                .keep_groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["bug-fixes"]
        );
        assert_eq!(
            proposal
                .revert_groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            vec!["auth"]
        );
    }
}
