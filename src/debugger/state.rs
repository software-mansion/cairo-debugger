use std::cmp::PartialEq;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use cairo_annotations::annotations::coverage::CodeLocation;
use cairo_lang_sierra::program::StatementIdx;
use cairo_vm::vm::vm_core::VirtualMachine;
use tracing::{debug, trace};

use crate::debugger::context::{Context, Line};
use crate::debugger::handler::StepAction;
use crate::debugger::state::call_stack::CallStack;
use crate::debugger::state::ui_state::UiState;

type SourcePath = String;

mod call_stack;
mod ui_state;

pub struct State {
    configuration_done: bool,
    execution_stopped: bool,
    breakpoints: HashMap<SourcePath, HashSet<StatementIdx>>,
    pub current_statement_idx: StatementIdx,
    pub call_stack: CallStack,
    last_breakpoint_hit: Option<BreakpointHit>,
    pub step_action: Option<StepAction>,
}

impl State {
    pub fn new() -> Self {
        Self {
            configuration_done: false,
            execution_stopped: false,
            breakpoints: HashMap::default(),
            current_statement_idx: StatementIdx(usize::MAX),
            call_stack: CallStack::default(),
            last_breakpoint_hit: None,
            step_action: None,
        }
    }

    pub fn update_state(&mut self, vm: &VirtualMachine, ctx: &Context) -> ControlFlow<()> {
        let current_pc = vm.get_pc();

        if current_pc.segment_index != 0 {
            // We cannot map pc to a sierra statement in such a case since we are before relocation.
            // Just stay at the previous pc.
            debug!("Skipping updating state for instruction with pc={current_pc}");
            return ControlFlow::Break(());
        }

        if let Some(idx) = ctx.statement_idx_for_pc(current_pc.offset)
            && idx != self.current_statement_idx
        {
            self.current_statement_idx = idx;
            self.call_stack.update_pre_step(self.current_statement_idx, ctx, vm);

            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }

    pub fn is_configuration_done(&self) -> bool {
        self.configuration_done
    }

    pub fn set_configuration_done(&mut self) {
        trace!("Configuration done");
        self.configuration_done = true;
    }

    pub fn is_execution_stopped(&self) -> bool {
        self.execution_stopped
    }

    pub fn stop_execution(&mut self) {
        trace!("Execution stopped");
        self.execution_stopped = true;
    }

    pub fn resume_execution(&mut self) {
        trace!("Execution resumed");
        self.execution_stopped = false;
    }

    pub fn verify_and_set_breakpoint(
        &mut self,
        source: SourcePath,
        line: Line,
        ctx: &Context,
    ) -> bool {
        let indexes = ctx.statement_idxs_for_breakpoint(Path::new(&source), line);

        if let Some(indexes) = indexes {
            debug!(
                "Setting breakpoint for file: {:?}, line: {:?}, idxs: {:?}",
                source, line, indexes
            );
            self.breakpoints.entry(source).or_default().extend(indexes);

            return true;
        }

        false
    }

    pub fn clear_breakpoints(&mut self, source: &SourcePath) {
        self.breakpoints.remove(source);
    }

    pub fn was_breakpoint_hit(&mut self, ctx: &Context, vm: &VirtualMachine) -> bool {
        if self
            .breakpoints
            .values()
            .flatten()
            .all(|statement_idx| *statement_idx != self.current_statement_idx)
        {
            return false;
        }

        let location = ctx
            .code_location_for_statement_idx(self.current_statement_idx)
            .expect("Breakpoint statement was expected to have corresponding code location");
        let ui_state = UiState::build(self, ctx, vm);
        let breakpoint_hit = Some(BreakpointHit { location, ui_state });

        // If we hit the same breakpoint and the ui state is the same,
        // there is nothing new to display to the user - so we don't stop there.
        if self.last_breakpoint_hit == breakpoint_hit {
            return false;
        }

        self.last_breakpoint_hit = breakpoint_hit;
        true
    }

    pub fn has_step_action_happened(&self, current_line: Line, ctx: &Context) -> bool {
        match &self.step_action {
            Some(StepAction::StepIn { prev_line }) if *prev_line != current_line => true,
            Some(StepAction::Next { prev_line, depth })
                if *depth >= self.call_stack.depth(self.current_statement_idx, ctx)
                    && *prev_line != current_line =>
            {
                true
            }
            Some(StepAction::StepOut { depth })
                if *depth > self.call_stack.depth(self.current_statement_idx, ctx) =>
            {
                true
            }
            _ => false,
        }
    }
}

#[derive(PartialEq)]
struct BreakpointHit {
    location: CodeLocation,
    /// State of the UI at the time when the breakpoint was hit.
    ui_state: UiState,
}
