use anyhow::Result;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

const FILES: &[(&str, &str)] = &[
    ("prime-goal/SKILL.md", include_str!("skills/goal/SKILL.md")),
    (
        "prime-goal/src/goal/__init__.py",
        include_str!("skills/goal/src/goal/__init__.py"),
    ),
    (
        "prime-refine/SKILL.md",
        include_str!("skills/refine/SKILL.md"),
    ),
    (
        "prime-refine/src/refine/__init__.py",
        include_str!("skills/refine/src/refine/__init__.py"),
    ),
    (
        "prime-rlm-heartbeat/SKILL.md",
        include_str!("skills/rlm-heartbeat/SKILL.md"),
    ),
    (
        "prime-rlm-heartbeat/src/rlm_heartbeat/__init__.py",
        include_str!("skills/rlm-heartbeat/src/rlm_heartbeat/__init__.py"),
    ),
    (
        "prime-agent-message/SKILL.md",
        include_str!("skills/agent-message/SKILL.md"),
    ),
    (
        "prime-agent-message/src/agent_message/__init__.py",
        include_str!("skills/agent-message/src/agent_message/__init__.py"),
    ),
    (
        "prime-agent-observe/SKILL.md",
        include_str!("skills/agent-observe/SKILL.md"),
    ),
    (
        "prime-agent-observe/src/agent_observe/__init__.py",
        include_str!("skills/agent-observe/src/agent_observe/__init__.py"),
    ),
    (
        "prime-compact/SKILL.md",
        include_str!("skills/compact/SKILL.md"),
    ),
    (
        "prime-compact/src/compact/__init__.py",
        include_str!("skills/compact/src/compact/__init__.py"),
    ),
    (
        "prime-websearch/SKILL.md",
        include_str!("skills/websearch/SKILL.md"),
    ),
    (
        "prime-websearch/src/websearch/__init__.py",
        include_str!("skills/websearch/src/websearch/__init__.py"),
    ),
    (
        "prime-websearch/src/websearch/websearch.py",
        include_str!("skills/websearch/src/websearch/websearch.py"),
    ),
    (
        "prime-skill-creator/SKILL.md",
        include_str!("skills/skill-creator/SKILL.md"),
    ),
    (
        "prime-skill-creator/src/skill_creator/__init__.py",
        include_str!("skills/skill-creator/src/skill_creator/__init__.py"),
    ),
    (
        "prime-skill-creator/references/python-skills.md",
        include_str!("skills/skill-creator/references/python-skills.md"),
    ),
];

/// Install the shipped Prime wrappers without overwriting user-owned skills.
pub fn install(skills_root: &Path) -> Result<()> {
    let mut prepared = HashSet::new();
    let mut skipped = HashSet::new();
    for (relative, contents) in FILES {
        let skill_dir = relative.split('/').next().expect("skill path is nonempty");
        if skipped.contains(skill_dir) {
            continue;
        }
        if !prepared.contains(skill_dir) {
            let directory = skills_root.join(skill_dir);
            if directory.exists() {
                skipped.insert(skill_dir);
                continue;
            }
            fs::create_dir_all(&directory)?;
            prepared.insert(skill_dir);
        }
        let path = skills_root.join(relative);
        let parent = path.parent().expect("skill asset has a parent");
        fs::create_dir_all(parent)?;
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        file.write_all(contents.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::install;

    #[test]
    fn bundle_install_is_repeatable_and_preserves_user_skill_directories() {
        let root = std::env::temp_dir().join(format!("prime-skills-{}", uuid::Uuid::new_v4()));
        let user_skill = root.join("prime-goal");
        std::fs::create_dir_all(&user_skill).unwrap();
        std::fs::write(user_skill.join("SKILL.md"), "user-owned").unwrap();

        install(&root).unwrap();
        install(&root).unwrap();

        assert_eq!(
            std::fs::read_to_string(user_skill.join("SKILL.md")).unwrap(),
            "user-owned"
        );
        assert!(!user_skill.join("src/goal/__init__.py").exists());
        assert!(root.join("prime-refine/src/refine/__init__.py").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}
