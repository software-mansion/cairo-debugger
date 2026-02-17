use std::path::Path;

use anyhow::{Result, anyhow};
use cairo_vm::vm::vm_core::VirtualMachine;
use dap::events::{Event, ExitedEventBody, StoppedEventBody};
use dap::prelude::Event::{Exited, Terminated};
use dap::prelude::{Request, ResponseBody};
use dap::types::StoppedEventReason;
use tracing::error;

use crate::connection::Connection;
use crate::debugger::context::{CasmDebugInfo, Context, Line};
use crate::debugger::handler::StepAction;
use crate::debugger::state::State;

mod call_stack;
pub mod context;
mod handler;
mod state;
mod vm;

/// According to [object references](https://microsoft.github.io/debug-adapter-protocol/overview#lifetime-of-objects-references).
const MAX_OBJECT_REFERENCE: i64 = (1 << 31) - 1;

/// According to [object references](https://microsoft.github.io/debug-adapter-protocol/overview#lifetime-of-objects-references).
const MIN_OBJECT_REFERENCE: i64 = 1;

pub struct CairoDebugger {
    connection: Connection,
    ctx: Context,
    state: State,
}

impl CairoDebugger {
    pub fn connect_and_initialize(
        sierra_path: &Path,
        casm_debug_info: CasmDebugInfo,
    ) -> Result<Self> {
        let connection = Connection::new()?;
        let ctx = Context::new(sierra_path, casm_debug_info)?;

        let mut debugger = Self { connection, ctx, state: State::new() };
        debugger.initialize()?;

        Ok(debugger)
    }

    fn initialize(&mut self) -> Result<()> {
        while !self.state.is_configuration_done() {
            // TODO(#35)
            let request = self.connection.next_request()?;
            self.process_request(request)?;
        }

        Ok(())
    }

    fn sync_with_vm(&mut self, vm: &VirtualMachine) -> Result<()> {
        self.state.update_state(vm, &self.ctx);

        self.maybe_handle_breakpoint_hit()?;
        self.maybe_handle_step_action()?;

        while let Some(request) = self.connection.try_next_request()? {
            self.process_request(request)?;

            if self.state.is_execution_stopped() {
                self.process_until_resume()?;
            }
        }

        Ok(())
    }

    fn process_until_resume(&mut self) -> Result<()> {
        while self.state.is_execution_stopped() {
            let request = self.connection.next_request()?;
            self.process_request(request)?;
        }

        Ok(())
    }

    fn process_request(&mut self, request: Request) -> Result<()> {
        let response = handler::handle_request(&request, &mut self.state, &self.ctx)?;
        let disconnected = matches!(response.response_body, ResponseBody::Disconnect);

        if let Some(event) = response.event {
            self.connection.send_event(event)?;
        }
        self.connection.send_success(request, response.response_body)?;

        if disconnected {
            // Returning an error is the easiest way to get the process to exit.
            return Err(anyhow!("Disconnect request received"));
        }

        Ok(())
    }

    fn maybe_handle_step_action(&mut self) -> Result<()> {
        let current_line =
            Line::create_from_statement_idx(self.state.current_statement_idx, &self.ctx);

        let stop = match &self.state.step_action {
            Some(StepAction::StepIn { prev_line }) if *prev_line != current_line => true,
            Some(StepAction::Next { prev_line, depth })
                if *depth
                    >= self.state.call_stack.depth(self.state.current_statement_idx, &self.ctx)
                    && *prev_line != current_line =>
            {
                true
            }
            Some(StepAction::StepOut { depth })
                if *depth
                    > self.state.call_stack.depth(self.state.current_statement_idx, &self.ctx) =>
            {
                true
            }
            _ => false,
        };

        if stop {
            self.state.step_action = None;
            self.pause_and_process_requests(StoppedEventReason::Step)?;
        }

        Ok(())
    }

    fn maybe_handle_breakpoint_hit(&mut self) -> Result<()> {
        if self.state.was_breakpoint_hit(&self.ctx) {
            self.pause_and_process_requests(StoppedEventReason::Breakpoint)?;
        }

        Ok(())
    }

    fn pause_and_process_requests(&mut self, reason: StoppedEventReason) -> Result<()> {
        self.state.stop_execution();
        self.connection.send_event(Event::Stopped(StoppedEventBody {
            reason,
            thread_id: Some(MAX_OBJECT_REFERENCE),
            all_threads_stopped: Some(true),
            // Breakpoint IDs are not set in `SetBreakpointsResponse`, hence we set them to `None` also here.
            // This would matter if we supported multiple breakpoints per line, but currently we don't.
            hit_breakpoint_ids: None,
            description: None,
            preserve_focus_hint: None,
            text: None,
        }))?;
        self.process_until_resume()
    }
}

impl Drop for CairoDebugger {
    fn drop(&mut self) {
        if let Err(err) = self.connection.send_event(Terminated(None)) {
            error!("Sending terminated event failed: {}", err);
        }

        // TODO(#34): Send correct exit code
        if let Err(err) = self.connection.send_event(Exited(ExitedEventBody { exit_code: 0 })) {
            error!("Sending exit event failed: {}", err);
        }
    }
}
