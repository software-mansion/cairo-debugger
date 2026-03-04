use dap::types::StackFrame;

use crate::debugger::context::Context;
use crate::debugger::state::State;

/// Represents the state of the debugger from the user's point of view.
/// E.g. `stack_trace` is visible to a user through [`dap::prelude::Command::StackTrace`] request.
#[derive(PartialEq)]
pub struct UiState {
    stack_trace: Vec<StackFrame>,
}

impl UiState {
    pub fn build(state: &State, ctx: &Context) -> Self {
        let stack_trace = state.call_stack.get_frames(state.current_statement_idx, ctx);
        UiState { stack_trace }
    }
}
