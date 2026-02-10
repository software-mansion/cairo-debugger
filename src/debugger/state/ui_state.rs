use cairo_vm::vm::vm_core::VirtualMachine;
use dap::types::{StackFrame, Variable};

use crate::debugger::context::Context;
use crate::debugger::state::State;

/// Represents the state of the debugger from the user's point of view.
/// E.g. `stack_trace` is visible to a user through [`dap::prelude::Command::StackTrace`] request.
#[derive(PartialEq)]
pub struct UiState {
    stack_trace: Vec<StackFrame>,
    values_of_variables: Vec<Variable>,
}

impl UiState {
    pub fn build(state: &State, ctx: &Context, vm: &VirtualMachine) -> Self {
        let stack_trace = state.call_stack.get_frames(state.current_statement_idx, ctx);

        // TODO: replace big var ref with enum.
        let values_of_variables =
            state.call_stack.get_variables(20000000000, state.current_statement_idx, ctx, vm);
        UiState { stack_trace, values_of_variables }
    }
}
