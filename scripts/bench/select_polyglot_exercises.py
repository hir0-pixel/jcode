#!/usr/bin/env python3
"""Generate the exercise selection for scripts/bench/polyglot.mjs.

Default: ALL exercises of all six languages (225), ordered by language then name ->
polyglot-exercises.json. `--count 40 --seed 42 --out polyglot-exercises-40.json` regenerates
the old python/rust/cpp seeded subset (polyglot.mjs `--set 40`). Deterministic; provenance only,
polyglot.mjs reads the committed json files.
"""
import argparse
import json
import random
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
POLYGLOT = ROOT.parent / "benchmarks" / "polyglot-benchmark"
ALL = ["cpp", "go", "java", "javascript", "python", "rust"]
LANGUAGES = ["python", "rust", "cpp"]  # the --count subset only


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
    ap.add_argument("--count", type=int, default=0, help="0 = all exercises")
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", default=str(Path(__file__).with_name("polyglot-exercises.json")))
    ap.add_argument("--langs", default="", help="comma list; with --per-lang, a seeded top-up of N exercises per language")
    ap.add_argument("--per-lang", type=int, default=0)
    args = ap.parse_args()
    if args.langs and args.per_lang:
        rng = random.Random(args.seed)
        langs = args.langs.split(",")
        chosen = {l: sorted(rng.sample(list_exercises(l), min(args.per_lang, len(list_exercises(l))))) for l in langs}
        total = sum(len(v) for v in chosen.values())
        out = {"seed": args.seed, "requested_count": args.per_lang * len(langs), "actual_count": total,
               "languages": langs, "per_language": args.per_lang, "exercises": chosen,
               "note": "top-up drawn before any results were seen; same seed as the 40-set"}
        Path(args.out).write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")
        print(f"wrote {args.out} ({total} exercises)")
        return
    langs = LANGUAGES if args.count else ALL
    chosen = select(args.count, args.seed) if args.count else {l: list_exercises(l) for l in ALL}
    total = sum(len(v) for v in chosen.values())
    out = {
        "seed": args.seed,
        "requested_count": args.count,
        "actual_count": total,
        "languages": langs,
        "exercises": chosen,
    }
    Path(args.out).write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {args.out} ({total} exercises)")


if __name__ == "__main__":
    main()
