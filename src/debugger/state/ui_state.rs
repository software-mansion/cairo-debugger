use cairo_vm::vm::vm_core::VirtualMachine;
use dap::types::StackFrame;

use crate::debugger::context::Context;
use crate::debugger::state::State;
use crate::debugger::state::call_stack::FunctionVariables;

/// Represents the state of the debugger from the user's point of view.
/// E.g. `stack_trace` is visible to a user through [`dap::prelude::Command::StackTrace`] request.
#[derive(PartialEq)]
pub struct UiState {
    stack_trace: Vec<StackFrame>,
    values_of_variables: FunctionVariables,
}

impl UiState {
    pub fn build(state: &State, ctx: &Context, vm: &VirtualMachine) -> Self {
        let stack_trace = state.call_stack.get_frames(state.current_statement_idx, ctx);
        let values_of_variables = state.call_stack.get_current_function_variables(ctx, vm);
        UiState { stack_trace, values_of_variables }
    }
}
