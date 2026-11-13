//! The `skill` tool: load a skill's instructions, or one of its files, when
//! a task calls for it (see [`crate::skills`]).
//!
//! The model is shown only skill names and descriptions each turn; this is
//! how it gets the rest. Loading returns the body of `SKILL.md`, the skill's
//! directory, and a listing of its other files, so it can go on to run a
//! script or open a reference.
//!
//! **It never asks**, like the other read-only tools — and so it must not be
//! a way around the ones that do. A global skill lives outside the project,
//! where `read` needs the user's approval; rather than make every reference
//! file of an installed skill a prompt, the optional `file` argument returns
//! one of the skill's *own* files, confined to the skill's directory
//! (resolved, so neither `..` nor a symlink leads out of it). Nothing else
//! outside the project becomes readable through it.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolSpec};
use crate::skills;

pub struct SkillTool {
    /// The user-global skill directories, fixed when the tool is built so
    /// tests can point it at temp directories instead of the real home.
    global: Vec<PathBuf>,
}

impl SkillTool {
    pub fn new(global: Vec<PathBuf>) -> Self {
        Self { global }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn requires_approval(&self, _args: &Value) -> bool {
        // Reads skill directories only; see the module docs.
        false
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: "Load a skill: instructions (often with scripts and reference \
                files) for one kind of task. The skills available are listed in the project \
                context by name and description; call this with a skill's name BEFORE doing a \
                task it covers, then follow what it returns. The result is the skill's \
                instructions, its directory, and a list of its other files. To read one of \
                those files, call again with `file` set to its listed path. With no name, \
                lists every skill."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill to load, as listed. Omit to list all skills."
                    },
                    "file": {
                        "type": "string",
                        "description": "Instead of the instructions, return this file of the skill (a path relative to the skill's directory, as listed when it was loaded)."
                    }
                }
            }),
        }
    }

    fn call(&self, ctx: &mut ToolCtx, args: Value) -> Result<String> {
        let found = skills::discover(ctx.project_root, &self.global);
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            if found.skills.is_empty() {
                return Ok("no skills are installed".to_string());
            }
            return Ok(found
                .skills
                .iter()
                .map(|s| format!("- {} — {}", s.name, s.description))
                .collect::<Vec<_>>()
                .join("\n"));
        };
        let skill = found
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
                anyhow!(
                    "no skill named {name:?}; available: {}",
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    }
                )
            })?;

        if let Some(file) = args.get("file").and_then(Value::as_str) {
            return skill.read(file);
        }

        let mut out = format!(
            "skill: {}\ndirectory: {}\n",
            skill.name,
            skill.dir.display()
        );
        let (files, truncated) = skill.files();
        if !files.is_empty() {
            out.push_str(&format!(
                "files (read one with {{\"name\": {:?}, \"file\": <path>}}; \
                 run scripts by their path under the directory):\n",
                skill.name
            ));
            for f in &files {
                out.push_str(&format!("  {f}\n"));
            }
            if truncated {
                out.push_str("  … and more\n");
            }
        }
        out.push('\n');
        out.push_str(&skill.body()?);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::testutil::{dirs, skill};
    use crate::tools::FileState;
    use std::path::Path;

    /// Call the tool the way the agent would for a tool that needs no
    /// approval.
    fn call(tool: &SkillTool, root: &Path, args: Value) -> Result<String> {
        let mut fstate = FileState::new();
        let mut ctx = ToolCtx {
            project_root: root,
            fstate: &mut fstate,
            approved: false,
        };
        tool.call(&mut ctx, args)
    }

    #[test]
    fn never_asks() {
        let tool = SkillTool::new(Vec::new());
        let args = json!({"name": "x", "file": "/etc/passwd"});
        assert!(!tool.requires_approval_in(&args, Path::new("/project")));
    }

    #[test]
    fn loads_the_body_with_directory_and_files() {
        let (root, global) = dirs("tool-load");
        let dir = skill(
            &global,
            "pdf",
            "read PDFs",
            "# PDF\n\nRun scripts/extract.py.",
        );
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/extract.py"), "print('hi')").unwrap();
        let tool = SkillTool::new(vec![global]);

        let out = call(&tool, &root, json!({"name": "pdf"})).unwrap();
        assert!(
            out.starts_with(&format!(
                "skill: pdf\ndirectory: {}\nfiles (",
                dir.display()
            )),
            "{out}"
        );
        assert!(out.contains("\n  scripts/extract.py\n"), "{out}");
        assert!(
            out.ends_with("\n\n# PDF\n\nRun scripts/extract.py."),
            "{out}"
        );
        assert!(
            !out.contains("description:"),
            "front matter is not repeated"
        );

        // A global skill's file, without approval — but only its own.
        let file = call(
            &tool,
            &root,
            json!({"name": "pdf", "file": "scripts/extract.py"}),
        );
        assert_eq!(file.unwrap(), "print('hi')");
        let escape = call(
            &tool,
            &root,
            json!({"name": "pdf", "file": "../pdf/../../project"}),
        );
        assert!(escape.is_err());
    }

    #[test]
    fn a_skill_with_no_other_files_is_just_its_body() {
        let (root, _) = dirs("tool-bare");
        let dir = skill(&root.join(".atelier/skills"), "tidy", "tidy up", "Do it.");
        let out = call(&SkillTool::new(Vec::new()), &root, json!({"name": "tidy"})).unwrap();
        assert_eq!(
            out,
            format!("skill: tidy\ndirectory: {}\n\nDo it.", dir.display())
        );
    }

    #[test]
    fn lists_skills_and_names_them_when_one_is_unknown() {
        let (root, global) = dirs("tool-list");
        let tool = SkillTool::new(vec![global.clone()]);
        assert_eq!(
            call(&tool, &root, json!({})).unwrap(),
            "no skills are installed"
        );
        let err = call(&tool, &root, json!({"name": "nope"})).unwrap_err();
        assert!(err.to_string().contains("available: (none)"), "{err}");

        skill(&root.join(".agents/skills"), "charts", "draw charts", "x");
        skill(&global, "pdf", "read PDFs", "y");
        assert_eq!(
            call(&tool, &root, Value::Null).unwrap(),
            "- charts — draw charts\n- pdf — read PDFs"
        );
        let err = call(&tool, &root, json!({"name": "nope"})).unwrap_err();
        assert!(err.to_string().contains("available: charts, pdf"), "{err}");
    }
}
