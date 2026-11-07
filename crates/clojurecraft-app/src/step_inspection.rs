//! Runs of the step inspection: every usable example executed with an instrumented copy of the
//! code in the sandbox, recording each step's intermediate values. Results are kept for the
//! session, with the specification and code they were recorded for.

use std::collections::HashMap;

use clojurecraft_domain::{ExampleId, FunctionDocument, FunctionId};
use clojurecraft_workflow::assistant::Execution;
use clojurecraft_workflow::intermediate::{self, Inspection, InspectionPlan};

use crate::execution::ExecutionState;

struct Running {
    run: u64,
    plan: Box<InspectionPlan>,
}

#[derive(Default)]
pub struct StepInspections {
    running: HashMap<FunctionId, Running>,
    results: HashMap<FunctionId, Inspection>,
    /// Why the last attempt to start could not run anything.
    problems: HashMap<FunctionId, String>,
    pub selected: HashMap<FunctionId, ExampleId>,
}

impl StepInspections {
    pub fn is_running(&self, function: FunctionId) -> bool {
        self.running.contains_key(&function)
    }

    pub fn result(&self, function: FunctionId) -> Option<&Inspection> {
        self.results.get(&function)
    }

    pub fn problem(&self, function: FunctionId) -> Option<&str> {
        self.problems.get(&function).map(String::as_str)
    }

    pub fn start(&mut self, doc: &FunctionDocument, execution: &mut ExecutionState) {
        if self.is_running(doc.id) {
            return;
        }
        self.problems.remove(&doc.id);
        let plan = match intermediate::plan(doc) {
            Ok(plan) => plan,
            Err(why) => {
                self.problems.insert(doc.id, why);
                return;
            }
        };
        match execution.start(plan.request.clone()) {
            Ok(run) => {
                self.running.insert(
                    doc.id,
                    Running {
                        run,
                        plan: Box::new(plan),
                    },
                );
            }
            Err(e) => {
                self.problems.insert(doc.id, e);
            }
        }
    }

    pub fn cancel(&mut self, function: FunctionId, execution: &mut ExecutionState) {
        if let Some(r) = self.running.remove(&function) {
            execution.cancel(r.run);
        }
    }

    /// Takes finished inspection runs out of `executed`.
    pub fn claim(&mut self, executed: &mut Vec<(u64, Execution)>) {
        executed.retain(|(run, execution)| {
            let Some(function) = self.running.iter().find(|(_, r)| r.run == *run).map(|(f, _)| *f) else {
                return true;
            };
            if let Some(r) = self.running.remove(&function) {
                let inspection =
                    intermediate::inspect(&r.plan, execution.outcome.as_ref().map_err(String::as_str));
                self.results.insert(function, inspection);
            }
            false
        });
    }
}
