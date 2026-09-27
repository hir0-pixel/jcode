# Python Skill Package Contract

```text
my-skill/
├── SKILL.md
└── src/
    └── my_skill/
        └── __init__.py
```

The import name uses underscores; the skill folder and frontmatter name use lowercase kebab case. Packages are imported from the persistent CPython worker with no installation step. Keep dependencies to Python's standard library. Engine operations must call `rlm.host_request` so work stays in the Rust engine.
