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
//! | `installer.autodetect` | `{ probeEc? }` | the install's inputs, worked out from the machine |
//! | `installer.plan` | `{ action, preferHooks?, force? }` | ordered steps, blockers, warnings |
//! | `installer.apply` | as above plus `confirm`, `auto`, `skipSteps`, `cpuMaxRpm`, `gpuMaxRpm`, `experimentalBoard`, `boardTable` | what was done (dry run unless `confirm`) |
//!
//! `auto` is what the wizard's install button sends: it fills in every
//! input the caller left unset from [`autodetect`], so nobody has to look
//! up their own board id or read the driver's tables. Anything sent
//! explicitly still wins.

pub mod autodetect;
pub mod detect;
pub mod ec;
pub mod execute;
pub mod patch;
pub mod plan;

use pyren_core::{Module, ModuleError, ModuleResult};
use serde::Deserialize;
use serde_json::{json, Value};

pub use autodetect::{Autodetected, ParamsEffect, RpmSource};
pub use ec::EcProbe;
pub use detect::Environment;
pub use execute::{execute, ExecuteContext, ExecutionReport};
pub use patch::{BoardParams, BoardTable, MaxRpm};
pub use plan::{plan, Action, Plan, PlanOptions, Strategy};

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
    /// Fill in whatever is left unset below from what the machine says
    /// about itself. Explicit values always win over detected ones.
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    cpu_max_rpm: Option<u32>,
    #[serde(default)]
    gpu_max_rpm: Option<u32>,
    /// Board id to add to one of the driver's tables, for untested boards.
    #[serde(default)]
    experimental_board: Option<String>,
    #[serde(default)]
    board_table: Option<BoardTable>,
    /// Ids of steps not to run. Only steps the plan marked `optional` may
    /// be named; anything else is refused rather than quietly ignored, so
    /// a caller cannot believe it opted out of `depmod`.
    #[serde(default)]
    skip_steps: Vec<String>,
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

            // Read-only, and unprivileged: it reads DMI, the driver's own
            // tables and the fan config, and changes nothing. Split out
            // from `apply` so the wizard can show what it worked out
            // before anything is authorised.
            "autodetect" => {
                #[derive(Debug, Default, Deserialize)]
                #[serde(rename_all = "camelCase", default)]
                struct AutodetectRequest {
                    /// Read the embedded controller to settle the
                    /// board-params variant, loading `ec_sys` if it is not
                    /// already there.
                    ///
                    /// Off by default because everything else this method
                    /// does is a read of something already present, and
                    /// loading a kernel module - however harmless, and this
                    /// one is loaded read-only - is not that. The wizard
                    /// asks for it, because clicking "install" is the
                    /// authorisation.
                    probe_ec: bool,
                }

                let request: AutodetectRequest = if params.is_null() {
                    AutodetectRequest::default()
                } else {
                    serde_json::from_value(params).map_err(|e| {
                        ModuleError::InvalidParams(format!("invalid autodetect request: {e}"))
                    })?
                };

                let env = Environment::detect();
                serde_json::to_value(Autodetected::detect(&env, request.probe_ec))
                    .map_err(|e| ModuleError::Internal(e.to_string()))
            }

            "plan" => {
                let request: PlanRequest = serde_json::from_value(params)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid plan request: {e}")))?;
                let env = Environment::detect();
                let plan = plan::plan(&env, request.action, PlanOptions::from(&request));
                serde_json::to_value(plan).map_err(|e| ModuleError::Internal(e.to_string()))
            }

            "apply" => {
                let request: ApplyRequest = serde_json::from_value(params)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid apply request: {e}")))?;

                let env = Environment::detect();
                let options =
                    PlanOptions { prefer_hooks: request.prefer_hooks, force: request.force };
                let plan = plan::plan(&env, request.action, options);

                if !plan.is_runnable() {
                    // Not the caller's mistake and not a permanent property
                    // of the machine either - the blockers say what would
                    // have to change first.
                    let reasons = pyren_core::Msg::join(
                        plan.blockers.iter().map(|b| b.message.clone()).collect(),
                        "; ",
                    )
                    .unwrap_or_else(|| pyren_core::Msg::literal(""));
                    return Err(ModuleError::localised(
                        pyren_core::ErrorKind::NotCapable,
                        pyren_core::msg!(
                            "installer.err.planNotRunnable",
                            { "reasons" => reasons.text },
                            "this plan cannot run: {reasons}"
                        ),
                    ));
                }

                for id in &request.skip_steps {
                    match plan.steps.iter().find(|step| &step.id == id) {
                        Some(step) if step.optional => {}
                        Some(_) => {
                            return Err(ModuleError::InvalidParams(format!(
                                "step '{id}' is not optional, so it cannot be skipped: the \
                                 plan tolerates a failure only in the steps it marks optional"
                            )))
                        }
                        None => {
                            return Err(ModuleError::InvalidParams(format!(
                                "no step '{id}' in this plan"
                            )))
                        }
                    }
                }

                // An `auto` apply probes: it is an install, and the
                // wizard has already surveyed the machine the same way.
                let detected = request.auto.then(|| Autodetected::detect(&env, true));

                let mut board = match (request.experimental_board, request.board_table) {
                    (Some(name), Some(table)) => Some((table, name)),
                    (Some(_), None) => {
                        // Which table a board goes into decides which EC
                        // offsets the driver reads; guessing it would give a
                        // driver that loads and then misreads the hardware.
                        return Err(ModuleError::InvalidParams(
                            "experimentalBoard also needs boardTable, since the table decides \
                             which thermal-profile code path the board uses"
                                .to_string(),
                        ));
                    }
                    _ => None,
                };
                let mut max_rpm = MaxRpm { cpu: request.cpu_max_rpm, gpu: request.gpu_max_rpm };

                if let Some(detected) = &detected {
                    board = board.or_else(|| detected.board());
                    max_rpm.cpu = max_rpm.cpu.or(detected.cpu_max_rpm);
                    max_rpm.gpu = max_rpm.gpu.or(detected.gpu_max_rpm);
                }

                let context = ExecuteContext {
                    max_rpm,
                    experimental_board: board,
                    daemon_binary: std::env::current_exe().ok(),
                    skip_steps: request.skip_steps,
                };

                let report = execute::execute(&plan, &env, &context, !request.confirm);
                serde_json::to_value(json!({
                    "plan": plan,
                    "report": report,
                    // Only present for an `auto` run, so a caller that
                    // filled everything in itself is not told what it
                    // would have been given.
                    "autodetected": detected,
                }))
                .map_err(|e| ModuleError::Internal(e.to_string()))
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}
