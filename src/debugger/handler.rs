use anyhow::{Result, anyhow, bail};
use cairo_vm::vm::vm_core::VirtualMachine;
use dap::events::{Event, StoppedEventBody};
use dap::prelude::{Command, Request, ResponseBody};
use dap::requests::{NextArguments, StepInArguments};
use dap::requests::{ScopesArguments, VariablesArguments};
use dap::responses::{
    ContinueResponse, EvaluateResponse, ScopesResponse, SetBreakpointsResponse,
    SetExceptionBreakpointsResponse, StackTraceResponse, ThreadsResponse, VariablesResponse,
};
use dap::types::{Breakpoint, Capabilities, StoppedEventReason, Thread};
use tracing::{error, trace};

use crate::debugger::MAX_OBJECT_REFERENCE;
use crate::debugger::context::{Context, Line};
use crate::debugger::state::State;

pub struct HandlerResponse {
    pub response_body: ResponseBody,
    pub event: Option<Event>,
}

impl From<ResponseBody> for HandlerResponse {
    fn from(response_body: ResponseBody) -> Self {
        Self { response_body, event: None }
    }
}

pub enum StepAction {
    StepIn { prev_line: Line },
    Next { depth: usize, prev_line: Line },
    StepOut { depth: usize },
}

impl HandlerResponse {
    #[must_use]
    pub fn with_event(mut self, event: Event) -> Self {
        self.event = Some(event);
        self
    }
}

pub fn handle_request(
    request: &Request,
    state: &mut State,
    ctx: &Context,
    vm: Option<&VirtualMachine>,
) -> Result<HandlerResponse> {
    match &request.command {
        // We have not yet decided if we want to support these.
        Command::Attach(_)
        | Command::ReverseContinue(_)
        | Command::StepBack(_)
        | Command::SetFunctionBreakpoints(_)
        | Command::BreakpointLocations(_)
        | Command::Cancel(_)
        | Command::Completions(_)
        | Command::DataBreakpointInfo(_)
        | Command::Disassemble(_)
        | Command::Goto(_)
        | Command::ExceptionInfo(_)
        | Command::GotoTargets(_)
        | Command::LoadedSources
        | Command::Modules(_)
        | Command::ReadMemory(_)
        | Command::RestartFrame(_)
        | Command::SetDataBreakpoints(_)
        | Command::Restart(_)
        | Command::TerminateThreads(_)
        | Command::Terminate(_)
        | Command::StepInTargets(_)
        | Command::SetVariable(_)
        | Command::SetInstructionBreakpoints(_)
        | Command::SetExpression(_)
        | Command::WriteMemory(_) => {
            // If we receive these with current capabilities, it is the client's fault.
            error!("Received unsupported request: {request:?}");
            bail!("Unsupported request");
        }
        Command::SetExceptionBreakpoints(_) => {
            // VS Code sometimes sends this request based on old user settings,
            // even if we disabled the feature in our initialization.
            //
            // We reply with "success" to keep the debug session running smoothly,
            // but we intentionally ignore the request and set no breakpoints.

            trace!("Ignoring SetExceptionBreakpoints request (feature disabled)");
            Ok(ResponseBody::SetExceptionBreakpoints(SetExceptionBreakpointsResponse {
                breakpoints: None,
            })
            .into())
        }

        // Initialize flow requests.
        Command::Initialize(args) => {
            trace!("Initialized a client: {:?}", args.client_name);
            Ok(HandlerResponse::from(ResponseBody::Initialize(Capabilities {
                supports_configuration_done_request: Some(true),
                ..Default::default()
            }))
            .with_event(Event::Initialized))
        }
        Command::Launch(_) => Ok(ResponseBody::Launch.into()),
        Command::ConfigurationDone => {
            // Start running the Cairo program here.
            state.set_configuration_done();
            Ok(ResponseBody::ConfigurationDone.into())
        }

        Command::Pause(_) => {
            state.stop_execution();
            Ok(HandlerResponse::from(ResponseBody::Pause).with_event(Event::Stopped(
                StoppedEventBody {
                    reason: StoppedEventReason::Pause,
                    thread_id: Some(MAX_OBJECT_REFERENCE),
                    description: None,
                    preserve_focus_hint: None,
                    text: None,
                    all_threads_stopped: Some(true),
                    hit_breakpoint_ids: None,
                },
            )))
        }
        Command::Continue(_) => {
            state.resume_execution();
            Ok(ResponseBody::Continue(ContinueResponse { all_threads_continued: Some(true) })
                .into())
        }

        Command::SetBreakpoints(args) => {
            let mut response_bps = Vec::new();
            if let Some(requested_bps) = &args.breakpoints {
                let source_path = args
                    .source
                    .path
                    .clone()
                    .ok_or_else(|| anyhow!("Source file path is missing"))?;

                state.clear_breakpoints(&source_path);

                for bp in requested_bps {
                    let is_valid = state.verify_and_set_breakpoint(
                        source_path.clone(),
                        // UI sends line numbers as 1-indexed, hence we subtract 1 here.
                        Line::new((bp.line - 1) as usize),
                        ctx,
                    );
                    response_bps.push(Breakpoint {
                        verified: is_valid,
                        source: Some(args.source.clone()),
                        line: Some(bp.line),
                        ..Default::default()
                    });
                }
            }
            Ok(ResponseBody::SetBreakpoints(SetBreakpointsResponse { breakpoints: response_bps })
                .into())
        }

        Command::Threads => {
            Ok(ResponseBody::Threads(ThreadsResponse {
                // Return a single thread.
                threads: vec![Thread { id: MAX_OBJECT_REFERENCE, name: "".to_string() }],
            })
            .into())
        }
        Command::StackTrace(_) => {
            let stack_frames = state.call_stack.get_frames(state.current_statement_idx, ctx);
            let total_frames = Some(stack_frames.len() as i64);
            Ok(ResponseBody::StackTrace(StackTraceResponse { stack_frames, total_frames }).into())
        }
        Command::Scopes(ScopesArguments { frame_id }) => {
            let scopes = state.call_stack.get_scopes_for_frame(*frame_id);
            Ok(ResponseBody::Scopes(ScopesResponse { scopes }).into())
        }
        Command::Variables(VariablesArguments { variables_reference, .. }) => {
            let variables = state.call_stack.get_variables(
                *variables_reference,
                state.current_statement_idx,
                ctx,
                vm.unwrap(),
            );
            Ok(ResponseBody::Variables(VariablesResponse { variables }).into())
        }

        // `vs-code` currently doesn't support choosing granularity, see: https://github.com/microsoft/vscode/issues/102236.
        // We assume granularity of line.
        Command::Next(NextArguments { .. }) => {
            // To handle a "step over" action, we set the step action to `Next`.
            // We record the current call stack depth. The debugger will resume execution
            // and only stop when it reaches a new line at the same or a shallower call stack depth.
            // This effectively "steps over" any function calls.
            let line = Line::create_from_statement_idx(state.current_statement_idx, ctx);

            state.step_action = Some(StepAction::Next {
                depth: state.call_stack.depth(state.current_statement_idx, ctx),
                prev_line: line,
            });

            state.resume_execution();
            Ok(ResponseBody::Next.into())
        }
        // `vs-code` currently doesn't support choosing granularity, see: https://github.com/microsoft/vscode/issues/102236.
        // We assume granularity of line.
        Command::StepIn(StepInArguments { .. }) => {
            // To handle a "step in" action, we set the step action to `StepIn`.
            // The debugger will resume execution and stop at the very next executable line,
            // which might be inside a function call.
            let line = Line::create_from_statement_idx(state.current_statement_idx, ctx);

            state.step_action = Some(StepAction::StepIn { prev_line: line });
            state.resume_execution();
            Ok(ResponseBody::StepIn.into())
        }
        Command::StepOut(_) => {
            // To handle a "step out" action, we set the step action to `StepOut`.
            // We record the current call stack depth. The debugger will resume execution
            // and only stop when it reaches a line in a shallower call stack depth, which
            // happens when the current function returns.
            let depth = state.call_stack.depth(state.current_statement_idx, ctx);
            if depth > 0 {
                state.step_action = Some(StepAction::StepOut { depth });
            }
            state.resume_execution();
            Ok(ResponseBody::StepOut.into())
        }
        Command::Source(_) => {
            todo!()
        }

        Command::Evaluate(_) => {
            Ok(ResponseBody::Evaluate(EvaluateResponse {
                // Return whatever since we cannot opt out of supporting this request.
                result: "".to_string(),
                type_field: None,
                presentation_hint: None,
                variables_reference: 0,
                named_variables: None,
                indexed_variables: None,
                memory_reference: None,
            })
            .into())
        }

        Command::Disconnect(_) => Ok(ResponseBody::Disconnect.into()),
    }
}
