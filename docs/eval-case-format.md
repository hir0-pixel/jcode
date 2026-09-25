# Eval case files (`JCODE_HOME/evals/*.json`)

Created by `POST /api/sovereign/observability/promote` with body `{ "run_id": "<id>" }`.

```json
{
  "version": 1,
  "source_run_id": "session:…",
  "session_id": "…",
  "kind": "invoke_agent",
  "prompt": "…",
  "final_answer": "…",
  "tool_calls": [{ "name": "…", "input": "…", "output": "…", "status": "complete" }],
  "created_at_ms": 0
}
```

Tool inputs/outputs appear only when `observability.json` has `"capture_content": true`.
