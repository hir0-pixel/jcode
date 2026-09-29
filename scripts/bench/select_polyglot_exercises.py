#!/usr/bin/env python3
"""Generate the fixed-seed exercise selection for scripts/bench/polyglot.mjs.

Deterministic: same seed + same sorted exercise names -> same selection, so
re-running this regenerates byte-identical scripts/bench/polyglot-exercises.json
(the committed file both arms and future reruns actually use; this script is
provenance, not something polyglot.mjs imports at run time).

Usage: python3 scripts/bench/select_polyglot_exercises.py [--count 40] [--seed 42]

Languages: python, rust, cpp only. java and javascript are excluded - verified
during the BUILD (not assumed): java's test runner needs a JVM (none installed;
`javac -version` prompts to install a runtime) and gradle (not installed, and
gradlew would download it); javascript's `npm test` needs jest/babel, which are
devDependencies nowhere vendored in this repo and would require `npm install`
(a network download). go was excluded per the task brief (not installed).
"""
import argparse
import json
import random
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
POLYGLOT = ROOT.parent / "benchmarks" / "polyglot-benchmark"
LANGUAGES = ["python", "rust", "cpp"]


def list_exercises(lang):
    d = POLYGLOT / lang / "exercises" / "practice"
    return sorted(p.name for p in d.iterdir() if p.is_dir())


def select(count, seed):
    rng = random.Random(seed)
    per_lang = {lang: list_exercises(lang) for lang in LANGUAGES}
    base = count // len(LANGUAGES)
    extra = count % len(LANGUAGES)
    chosen = {}
    for i, lang in enumerate(LANGUAGES):
        n = base + (1 if i < extra else 0)
        pool = per_lang[lang]
        n = min(n, len(pool))
        chosen[lang] = sorted(rng.sample(pool, n))
    return chosen


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=40)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", default=str(Path(__file__).with_name("polyglot-exercises.json")))
    args = ap.parse_args()
    chosen = select(args.count, args.seed)
    total = sum(len(v) for v in chosen.values())
    out = {
        "seed": args.seed,
        "requested_count": args.count,
        "actual_count": total,
        "languages": LANGUAGES,
        "excluded_languages": {
            "go": "not installed (re-verified 2026-09-29)",
            "java": "no JVM installed and gradle/gradlew would need a download",
            "javascript": "jest/babel devDependencies not vendored; npm test needs npm install (network)",
        },
        "exercises": chosen,
    }
    Path(args.out).write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {args.out} ({total} exercises)")


if __name__ == "__main__":
    main()
