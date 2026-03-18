//! `hc-scripting` — sandboxed Rhai script runtime.

use anyhow::{anyhow, Result};
use rhai::{Dynamic, Engine, Scope};
use tracing::debug;

/// A configured Rhai engine ready to evaluate HomeCore scripts.
pub struct ScriptRuntime {
    engine: Engine,
}

impl ScriptRuntime {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        engine.set_max_operations(100_000);
        engine.set_max_call_levels(32);
        engine.set_max_string_size(1024 * 64);
        engine.set_max_array_size(4096);
        engine.set_max_map_size(1024);
        Self { engine }
    }

    /// Evaluate a boolean expression (`Condition::ScriptExpression`).
    pub fn eval_condition(&self, script: &str) -> Result<bool> {
        let result: bool = self
            .engine
            .eval_expression(script)
            .map_err(|e| anyhow!("Condition script error: {e}"))?;
        debug!(%script, %result, "Condition script evaluated");
        Ok(result)
    }

    /// Execute a script (`Action::RunScript`).
    pub fn run_action(&self, script: &str) -> Result<Dynamic> {
        let mut scope = Scope::new();
        let result = self
            .engine
            .eval_with_scope::<Dynamic>(&mut scope, script)
            .map_err(|e| anyhow!("Action script error: {e}"))?;
        debug!(%script, "Action script executed");
        Ok(result)
    }

    /// Call a named transform function (used by `hc-topic-map`).
    pub fn call_transform(&self, ast: &rhai::AST, fn_name: &str, payload: &str) -> Result<String> {
        let result: Dynamic = self
            .engine
            .call_fn(&mut Scope::new(), ast, fn_name, (payload.to_string(),))
            .map_err(|e| anyhow!("Transform '{fn_name}' error: {e}"))?;
        Ok(result.to_string())
    }

    /// Compile a script to an AST for repeated invocation.
    pub fn compile(&self, script: &str) -> Result<rhai::AST> {
        self.engine
            .compile(script)
            .map_err(|e| anyhow!("Script compilation error: {e}"))
    }
}

impl Default for ScriptRuntime {
    fn default() -> Self {
        Self::new()
    }
}
