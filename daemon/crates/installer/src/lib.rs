//! Driver and service installer, ported from the source project's
//! `install_driver.sh` plus the install paths in `omen_logic.py`.
//!
//! The port is deliberately split into **inspect → plan → apply** rather
//! than one imperative script. Installing means unloading a kernel module,
//! replacing a file under `/lib/modules` and regenerating the initramfs;
//! a user should be able to see exactly what will run before authorising
//! it, and a rendered plan is also something that can be pasted into a bug
//! report. It also puts all the decision-making in a pure function that is
//! testable on any machine, HP or not.
//!
//! | method | params | result |
//! |---|---|---|
//! | `installer.inspect` | none | what this machine has, and whether the patch is needed |
//! | `installer.plan` | `{ action, preferHooks?, force? }` | ordered steps, blockers, warnings |
//! | `installer.apply` | as above plus `confirm`, `maxRpm`, `board` | what was done (dry run unless `confirm`) |

pub mod detect;
pub mod execute;
pub mod patch;
pub mod plan;

use omen_hub_core::{Module, ModuleError, ModuleResult};
use serde::Deserialize;
use serde_json::{json, Value};

pub use detect::Environment;
pub use execute::{ExecuteContext, ExecutionReport};
pub use patch::{BoardParams, BoardTable, MaxRpm};
pub use plan::{Action, Plan, PlanOptions, Strategy};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanRequest {
    action: Action,
    #[serde(default)]
    prefer_hooks: bool,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyRequest {
    action: Action,
    #[serde(default)]
    prefer_hooks: bool,
    #[serde(default)]
    force: bool,
    /// Must be explicitly true to touch the system. Anything else is a
    /// dry run, so a mis-sent message cannot replace a kernel module.
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    cpu_max_rpm: Option<u32>,
    #[serde(default)]
    gpu_max_rpm: Option<u32>,
    /// Board id to add to one of the driver's tables, for untested boards.
    #[serde(default)]
    experimental_board: Option<String>,
    #[serde(default)]
    board_table: Option<BoardTable>,
}

impl From<&PlanRequest> for PlanOptions {
    fn from(request: &PlanRequest) -> Self {
        Self { prefer_hooks: request.prefer_hooks, force: request.force }
    }
}

#[derive(Default)]
pub struct InstallerModule;

impl InstallerModule {
    pub fn new() -> Self {
        Self
    }
}

impl Module for InstallerModule {
    fn id(&self) -> &'static str {
        "installer"
    }

    /// Always available: inspecting what is installed, and explaining why
    /// something can't be installed, are useful answers on any machine.
    fn is_supported(&self) -> bool {
        true
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "inspect" => {
                let env = Environment::detect();
                Ok(json!({
                    "environment": env,
                    "patchNeeded": env.patch_needed(),
                }))
            }

            "plan" => {
                let request: PlanRequest = serde_json::from_value(params)
                    .map_err(|e| ModuleError::Other(format!("invalid plan request: {e}")))?;
                let env = Environment::detect();
                let plan = plan::plan(&env, request.action, PlanOptions::from(&request));
                serde_json::to_value(plan).map_err(|e| ModuleError::Other(e.to_string()))
            }

            "apply" => {
                let request: ApplyRequest = serde_json::from_value(params)
                    .map_err(|e| ModuleError::Other(format!("invalid apply request: {e}")))?;

                let env = Environment::detect();
                let options =
                    PlanOptions { prefer_hooks: request.prefer_hooks, force: request.force };
                let plan = plan::plan(&env, request.action, options);

                if !plan.is_runnable() {
                    let reasons: Vec<&str> =
                        plan.blockers.iter().map(|b| b.message.as_str()).collect();
                    return Err(ModuleError::Other(format!(
                        "this plan cannot run: {}",
                        reasons.join("; ")
                    )));
                }

                let board = match (request.experimental_board, request.board_table) {
                    (Some(name), Some(table)) => Some((table, name)),
                    (Some(_), None) => {
                        // Which table a board goes into decides which EC
                        // offsets the driver reads; guessing it would give a
                        // driver that loads and then misreads the hardware.
                        return Err(ModuleError::Other(
                            "experimentalBoard also needs boardTable, since the table decides \
                             which thermal-profile code path the board uses"
                                .to_string(),
                        ));
                    }
                    _ => None,
                };

                let context = ExecuteContext {
                    max_rpm: MaxRpm { cpu: request.cpu_max_rpm, gpu: request.gpu_max_rpm },
                    experimental_board: board,
                    daemon_binary: std::env::current_exe().ok(),
                };

                let report = execute::execute(&plan, &env, &context, !request.confirm);
                serde_json::to_value(json!({ "plan": plan, "report": report }))
                    .map_err(|e| ModuleError::Other(e.to_string()))
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}
