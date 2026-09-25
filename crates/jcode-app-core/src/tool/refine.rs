//! `refine`: model-callable Continual Harness refinement (sovereign engine
//! only). Mirrors Prime Agent's `refine.run()`/`refine.status()` REPL calls,
//! but as a normal tool: it only *schedules* a refinement (never applies one
//! mid-turn), which the gateway's automatic learning pass (`learn::pass`)
//! runs once this turn goes idle, exactly like Prime's turn-end scheduling.

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct RefineTool;

#[async_trait]
impl Tool for RefineTool {
    fn name(&self) -> &str {
        "refine"
    }

    fn description(&self) -> &str {
        "Schedule a Continual Harness refinement from this session (a durable prompt addendum, memory reference, \
         skill, or subagent spec) or check whether one is pending. Applied automatically once this turn ends, never \
         mid-turn; evidence-gated against this session's own messages. Use when the user states a durable preference \
         or correction you should carry into future sessions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "op": { "type": "string", "enum": ["run", "status"], "description": "\"run\" schedules a refinement; \"status\" reports whether one is pending" },
                "instructions": { "type": "string", "description": "what to refine (optional; the model deciding at turn end may use its own judgement)" },
                "global": { "type": "boolean", "description": "scope the refinement to every session instead of just this one" }
            },
            "required": ["op"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let home = jcode_base::storage::jcode_dir()?;
        let store = sovereign_prime::entries::EntryStore::open_cached(&home)?;
        match input["op"].as_str().unwrap_or("run") {
            "status" => {
                let pending = store.refine_pending(&ctx.session_id)?;
                Ok(ToolOutput::new(if pending {
                    "A refinement is scheduled and will run when this turn ends.".to_string()
                } else {
                    "No refinement is pending.".to_string()
                }))
            }
            _ => {
                let instructions = input["instructions"].as_str();
                let global = input["global"].as_bool().unwrap_or(false);
                store.schedule_refine(&ctx.session_id, instructions, global)?;
                Ok(ToolOutput::new(format!(
                    "Scheduled ({}); will run when this turn ends.",
                    if global { "global" } else { "this session" }
                )))
            }
        }
    }
}
