//! Skills provider: tells the model which skills exist (see
//! [`crate::skills`]).
//!
//! Only `name — description` is injected, one line per skill, with a line on
//! how to load one. The instructions themselves stay on disk until the model
//! calls the `skill` tool, so an installed skill costs a line, not a page.
//!
//! The list has its own small budget ([`MAX_LIST_CHARS`]) on top of the
//! overall one applied by the caller: a home directory full of skills must
//! not crowd out the git status and diagnostics ranked beneath it. Project
//! skills are discovered first, so they are the ones that survive the cut.

use super::{ContextItem, ContextProvider};
use std::path::{Path, PathBuf};

/// Most characters of skill lines to inject (about 300 tokens). Past it the
/// item says how many were left out and how to see them.
const MAX_LIST_CHARS: usize = 1200;

/// Most characters of one description; a skill's author may have written a
/// paragraph, and one skill must not use up the list.
const MAX_DESCRIPTION_CHARS: usize = 200;

/// How to use the list. Kept short: it is sent with every list.
const PREAMBLE: &str = "Skills available, as `name — description`. Each is a set of \
    instructions (often with scripts) for one kind of task. Before starting a task that a \
    skill covers, load it by calling the `skill` tool with {\"name\": ...} and follow it.";

/// Reports the available skills, or `None` when there are none.
pub struct SkillsProvider {
    /// The user-global skill directories, fixed at construction so tests can
    /// use temp directories instead of the real home.
    global: Vec<PathBuf>,
}

impl SkillsProvider {
    pub fn new(global: Vec<PathBuf>) -> Self {
        Self { global }
    }
}

impl ContextProvider for SkillsProvider {
    fn name(&self) -> &str {
        "skills"
    }

    fn gather(&self, root: &Path) -> Option<ContextItem> {
        let skills = crate::skills::discover(root, &self.global).skills;
        if skills.is_empty() {
            return None;
        }

        let mut body = String::from(PREAMBLE);
        let mut used = 0;
        let mut shown = 0;
        for s in &skills {
            let mut description: String =
                s.description.chars().take(MAX_DESCRIPTION_CHARS).collect();
            if description.len() < s.description.len() {
                description.push('…');
            }
            let line = format!("\n- {} — {description}", s.name);
            used += line.chars().count();
            if used > MAX_LIST_CHARS {
                break;
            }
            body.push_str(&line);
            shown += 1;
        }
        if shown < skills.len() {
            body.push_str(&format!(
                "\n… and {} more; call `skill` with no name to list them all",
                skills.len() - shown
            ));
        }
        // Below the project instructions (250) and the memory index (240),
        // above everything that describes the repository's current state.
        Some(ContextItem::new("skills", body, 230))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::testutil::{dirs, skill};

    #[test]
    fn none_when_there_are_no_skills() {
        let (root, global) = dirs("ctx-empty");
        assert!(SkillsProvider::new(vec![global]).gather(&root).is_none());
        assert!(SkillsProvider::new(Vec::new()).gather(&root).is_none());
    }

    #[test]
    fn lists_names_and_descriptions_without_bodies() {
        let (root, global) = dirs("ctx-list");
        skill(
            &root.join(".atelier/skills"),
            "release",
            "cut a release",
            "body that must not be injected",
        );
        skill(&global, "pdf", "read PDFs", "nor this one");

        let item = SkillsProvider::new(vec![global]).gather(&root).unwrap();
        assert_eq!(item.title, "skills");
        assert_eq!(item.priority, 230);
        assert!(item.body.starts_with(PREAMBLE));
        assert!(
            item.body
                .ends_with("\n- release — cut a release\n- pdf — read PDFs")
        );
        assert!(!item.body.contains("must not be injected"));
        assert!(!item.body.contains("nor this one"));
    }

    #[test]
    fn a_long_list_is_cut_with_a_count() {
        let (root, _) = dirs("ctx-budget");
        let description = "d".repeat(500);
        for i in 0..20 {
            skill(
                &root.join(".claude/skills"),
                &format!("skill-{i:02}"),
                &description,
                "body",
            );
        }
        let body = SkillsProvider::new(Vec::new()).gather(&root).unwrap().body;
        let shown = body.matches("\n- skill-").count();
        assert!(shown > 0 && shown < 20, "shown {shown}");
        assert!(body.contains(&format!("… and {} more", 20 - shown)));
        // Each description was cut too.
        assert!(body.contains(&format!("{}…", "d".repeat(MAX_DESCRIPTION_CHARS))));
        assert!(!body.contains(&"d".repeat(MAX_DESCRIPTION_CHARS + 1)));
        assert!(body.chars().count() < PREAMBLE.chars().count() + MAX_LIST_CHARS + 100);
    }
}
