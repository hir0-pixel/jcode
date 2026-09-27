---
name: prime-skill-creator
description: Create reusable markdown skills or stdlib-only Python skill packages in Akira's shared skill store. Use when a workflow should become importable REPL code.
---

# Skill Creator

A skill lives in `~/.jcode/skills/<skill-name>/` and starts with a `SKILL.md` containing valid `name` and `description` frontmatter. Python packages may add `src/<import_name>/__init__.py`; the REPL adds each installed skill's `src` directory to its import path. Prefer markdown unless the skill needs a callable Python function. Use Python standard library and the `rlm.host_request` bridge for engine operations. Do not add dependencies or call external services from a skill.
