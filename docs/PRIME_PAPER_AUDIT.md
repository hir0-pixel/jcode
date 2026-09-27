# Prime Agent paper: benchmark and component audit

Source: [Prime Agent: A Self-Improving RLM Harness, arXiv:2608.23552v1](https://arxiv.org/html/2608.23552v1), especially §§2–3 and the conclusion. The paper does not include a component ablation table. It says targeted training on RLM and Continual Harness is still needed to isolate their contributions. Therefore no individual component can be assigned a causal score delta from this paper.

## Reported results and what they establish

| Evaluation | Paper result | What the paper attributes it to | Isolated component contribution |
|---|---|---|---|
| ARC-AGI-3 | Prime RHAE Best@1 rises from 30% to 95.5% in the abstract; the introduction rounds the best result to 95% | The complete Prime Agent harness and model-controlled test-time scaling | None; no component ablation. Some reference points are external because native-harness reruns were below published scores |
| Long-context suite | Table 1 reports Prime and comparison harnesses for OOLONG, OOLONG-Pairs, OBLIQ-Bench, LongBench Pro/v2, ManyIH Coding/IF, LongCoT-Mini, and EmulatorBench | Persistent REPL plus programmatic context search, transformation, aggregation, and revisiting | None; the table compares full harnesses, not REPL-on/off variants |
| nanoGPT speedrun | Harness choice has little effect on final records relative to experiment noise; Prime models use more out-of-loop computation | Persistent REPL supports experiments outside the training script; DeepSeek V4 Pro performs about 6x more such experiments per training run than Claude Code; Kimi K3 performs about 90 screening experiments and all 19 validated records through a programmatic probe | Usage differences are reported, but no causal score delta is isolated for the REPL |
| PMPP-Hard GPU kernels | Prime and native harnesses are close in solve rates; ordering reverses between model groups; Prime uses substantially fewer tokens at similar performance | Persistent programmable edit/compile/correctness/profile loop | Token reduction is qualitative here; no numeric component ablation |
| EmulatorBench | Preliminary Table 1 scores are reported across 16 reconstructions; two selected systems (Genesis and Game Boy Color) are successfully reproduced | Programmatic long-context software construction inside the standardized harness | No isolated contribution; the paper notes some Opus runs failed despite successful tool calls |
| Factorio | Seven-day run: 24/196 technologies, 71% progress on advanced-circuit research, 23.4M output tokens, 633 depth-one child agents across 149 waves, max concurrency 7 | Iterative refinement and dedicated subagents supported continued technology progression and parallelized work | No ablation. The paper also documents a refinement safety failure that preserved an exploit |
| MazeBench | Exploration progress and token spend are plotted for Prime and native harnesses | Persistent interaction and long-horizon execution | No component-level numeric delta stated in the text |

## Component map for Akira

| Paper component | Akira implementation | Status |
|---|---|---|
| Persistent Python REPL and programmatic context | CPython worker, persistent variables, `load`, `llm_query`, timeout, RSS watchdog, macOS sandbox | Core present; performance budget bench missing |
| Recursive model calls | Host-mediated `llm_query` | Present |
| Continual Harness state and refinement | Rust `sovereign-prime` entries, local/global scope, rollback, rationale, `/refine` | Present in Rust; Prime-compatible executable Python skills are incomplete |
| Recursive subagents | Compact `delegate` on existing swarm internals | Spawn works; REPL await/result lifecycle incomplete |
| Direct agent messaging | Existing `CommunicateTool`, plus compact `agent_message` send/read/list tool | Present on existing swarm paths; role-addressed family observation is incomplete |
| Session persistence and recovery | Gateway session and control stores | Paths exist; combined goal/heartbeat/subagent reattach e2e is missing |
| Verification, termination, accounting | Rust gateway observability, replay, session controls | Present in Rust; parity benchmark not complete |
| Autonomous mode, goals, and heartbeats | Rust `/autonomous`, `/goal`, `/heartbeat` | Present; Prime-specific budget/result semantics and internal RLM heartbeat are incomplete |
| Human Agents View | Hermes desktop session and activity views | Not audited here as a performance component; combined live continuity test is missing |

## Follow-up audit result

The paper names no numeric ablation deltas for the REPL, Continual Harness,
subagents, messaging, refinement, or persistence. The strongest explicit
component attributions are qualitative: persistent REPL use enabled extra
out-of-loop experiments and lower token use at similar GPU-kernel performance;
iterative refinement plus dedicated subagents supported Factorio progress and
parallelism. The nanoGPT comparison says final records were within experiment
noise. Treating any of these as an isolated percentage gain would overstate the
paper.

The three-arm harness is present in `scripts/bench/abeval.py` and exposes
`--arm {hermes,sovereign,prime}` with the common task registry, local counting
proxy, and isolated homes. `python3 scripts/bench/abeval.py --help` and
`run --help` were checked in this pass. The Prime CLI is absent from `/tmp`, so
no new three-arm run or new performance-budget measurements were possible.
Akira's remaining component gaps are the ones listed as partial/gap in
`docs/PRIME_PARITY.md`; the paper does not provide evidence to fill those gaps
by inference.

The paper's outcome numbers describe full-system runs and are not predictions for Akira. The Prime RPC benchmark arm is implemented in `scripts/bench/abeval.py`; a single local task passed through the shared counting proxy. The full nine-task arm has not been run.
