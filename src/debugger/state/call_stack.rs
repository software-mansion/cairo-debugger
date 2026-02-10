use std::collections::HashMap;
use std::iter;
use std::path::Path;

use cairo_annotations::annotations::coverage::{CodeLocation, SourceFileFullPath};
use cairo_annotations::annotations::profiler::FunctionName;
use cairo_lang_sierra::program::{GenBranchTarget, StatementIdx};
use cairo_vm::vm::vm_core::VirtualMachine;
use dap::types::{Scope, ScopePresentationhint, StackFrame, Variable};
use dap::types::{Source, StackFramePresentationhint};

use crate::debugger::MIN_OBJECT_REFERENCE;
use crate::debugger::context::Context;
use crate::debugger::state::call_stack::variables::{RegistersValues, get_values_of_variables};

mod variables;

#[derive(Default)]
pub struct CallStack {
    /// Stack of Cairo function frames and values of variables in frames corresponding
    /// to these functions.
    ///
    /// 1. Each function frame corresponds to a Cairo function that is currently on the Cairo call
    ///    stack.
    ///    This includes inlined functions, even though they are not present on the call stack
    ///    if one looks from Sierra/CASM POV.
    ///
    /// 2. The stack is divided into substacks.
    ///    Each substack corresponds to a Sierra function call statement that is currently
    ///    on the Sierra call stack.
    ///    The substack contains zero to many frames of inlined Cairo functions together
    ///    with exactly one non-inlined Cairo function frame at the end of the substack.
    ///
    /// 3. The stack does ***not*** contain a substack corresponding to the current statement.
    ///
    /// [Object references](https://microsoft.github.io/debug-adapter-protocol/overview#lifetime-of-objects-references):
    /// object reference for each stack frame is equal to its `1 + 2 * flat_index`
    /// where `flat_index` is its position in the flattened vector (vector of tuples).
    /// For the variables' scope, the object reference is equal to `2 + 2 * flat_index`.
    call_frames_and_vars:
        Vec<(SubStack, PostStatementsRegisters, StatementsAwaitingBranchEntrances)>,

    /// Mapping from sierra statements executed during the current function to values of registers
    /// after entering the branch of the statement.
    post_statements_registers: PostStatementsRegisters,

    // TODO: keep a map of statements awaiting their branch to be entered to update post statements registers.
    statements_awaiting_branch_entrances: StatementsAwaitingBranchEntrances,

    /// Modification that should be applied to the stack when a new sierra statement is reached.
    ///
    /// This field is there to ensure that a correct stack trace is returned when a current
    /// statement maps to a function call or a return statement.
    /// The stack should be modified ***after*** such a statement is executed.
    action_on_new_statement: Option<Action>,
}

type SubStack = Vec<(StackFrame, FunctionVariables)>;
type PostStatementsRegisters =
    HashMap<StatementIdx, (GenBranchTarget<StatementIdx>, RegistersValues)>;
type StatementsAwaitingBranchEntrances =
    HashMap<StatementIdx, (GenBranchTarget<StatementIdx>, StatementIdx)>;

enum Action {
    Push(SubStack),
    Pop,
}

impl CallStack {
    pub fn depth(&self, statement_idx: StatementIdx, ctx: &Context) -> usize {
        self.flat_length() + self.build_stack_frames(ctx, statement_idx).count()
    }

    pub fn update_post_step(&mut self) {
        // We can be sure that the next `statement_idx` is different from the one which was the arg
        // when `action_on_new_statement` was set.
        // The reason is that both function call and return in sierra compile to one CASM instruction each.
        // https://github.com/starkware-libs/cairo/blob/20eca60c88a35f7da13f573b2fc68818506703a9/crates/cairo-lang-sierra-to-casm/src/invocations/function_call.rs#L46
        // https://github.com/starkware-libs/cairo/blob/d52acf845fc234f1746f814de7c64b535563d479/crates/cairo-lang-sierra-to-casm/src/compiler.rs#L533
        match self.action_on_new_statement.take() {
            Some(Action::Push(frames_and_variables)) => {
                // TODO(#16)
                let post_statements_registers = std::mem::take(&mut self.post_statements_registers);
                let statements_awaiting_branch_entrances =
                    std::mem::take(&mut self.statements_awaiting_branch_entrances);
                self.call_frames_and_vars.push((
                    frames_and_variables,
                    post_statements_registers,
                    statements_awaiting_branch_entrances,
                ));
            }
            Some(Action::Pop) => {
                if let Some((_, post_statements_registers, statements_awaiting_branch_entrances)) =
                    self.call_frames_and_vars.pop()
                {
                    self.post_statements_registers = post_statements_registers;
                    self.statements_awaiting_branch_entrances =
                        statements_awaiting_branch_entrances;
                }
            }
            None => {}
        }
    }

    pub fn update_pre_step(
        &mut self,
        statement_idx: StatementIdx,
        ctx: &Context,
        vm: &VirtualMachine,
    ) {
        if let Some(branches) = ctx.branches_for_statement(statement_idx) {
            let branches = branches.into_iter().map(|branch| {
                let branch_entrance_idx = match &branch {
                    GenBranchTarget::Fallthrough => StatementIdx(statement_idx.0 + 1),
                    GenBranchTarget::Statement(idx) => *idx,
                };

                (branch_entrance_idx, (branch, statement_idx))
            });
            self.statements_awaiting_branch_entrances.extend(branches);
        }

        for idx in ctx.previous_statements_with_same_start_offset(statement_idx) {
            if let Some((branch_target, awaiting_idx)) =
                self.statements_awaiting_branch_entrances.get(&idx)
            {
                self.post_statements_registers.insert(
                    *awaiting_idx,
                    (
                        branch_target.clone(),
                        RegistersValues { ap: vm.get_ap().offset, fp: vm.get_fp().offset },
                    ),
                );
            }
        }

        if ctx.is_function_call_statement(statement_idx) {
            let frames: Vec<_> = self.build_stack_frames(ctx, statement_idx).collect();

            // TODO: handle variables of inlined functions.
            let vars = iter::repeat_n(FunctionVariables::default(), frames.len() - 1).chain(
                iter::once(FunctionVariables {
                    names_to_values: get_values_of_variables(
                        ctx,
                        statement_idx,
                        vm,
                        &self.post_statements_registers,
                    ),
                }),
            );

            let frames_and_vars = frames.into_iter().zip(vars).collect();

            self.action_on_new_statement = Some(Action::Push(frames_and_vars));
        } else if ctx.is_return_statement(statement_idx) {
            self.action_on_new_statement = Some(Action::Pop);
        }
    }

    pub fn get_frames(&self, statement_idx: StatementIdx, ctx: &Context) -> Vec<StackFrame> {
        self.call_frames_and_vars
            .iter()
            .flat_map(|substack| &substack.0)
            .map(|(frame, _)| frame)
            .cloned()
            .chain(self.build_stack_frames(ctx, statement_idx))
            // DAP expects frames to start from the most nested element.
            .rev()
            .collect()
    }

    pub fn get_scopes_for_frame(&self, frame_id: i64) -> Vec<Scope> {
        let scope = Scope {
            name: "Locals".to_string(),
            variables_reference: frame_id + 1,
            presentation_hint: Some(ScopePresentationhint::Locals),
            ..Default::default()
        };
        vec![scope]
    }

    pub fn get_variables(
        &self,
        variables_reference: i64,
        statement_idx: StatementIdx,
        ctx: &Context,
        vm: &VirtualMachine,
    ) -> Vec<Variable> {
        let flat_index = (variables_reference / 2 - 1) as usize;

        let names_to_values = if flat_index >= self.flat_length() {
            // Build them on demand.
            get_values_of_variables(ctx, statement_idx, vm, &self.post_statements_registers)
        } else {
            self.call_frames_and_vars
                .iter()
                .flat_map(|substack| &substack.0)
                .map(|(_, vars)| vars)
                .nth(flat_index)
                .unwrap()
                .names_to_values
                .clone()
        };

        names_to_values
            .into_iter()
            .map(|(name, value)| Variable {
                name,
                value,
                variables_reference: 0,
                ..Default::default()
            })
            .collect()
    }

    /// Builds a vector of stack frames, ordered from the least nested to the most nested element.
    fn build_stack_frames<'a>(
        &'a self,
        ctx: &'a Context,
        statement_idx: StatementIdx,
    ) -> Box<dyn DoubleEndedIterator<Item = StackFrame> + 'a> {
        let Some(code_locations) = ctx.code_locations_for_statement_idx(statement_idx) else {
            return Box::new(vec![self.unknown_frame()].into_iter());
        };

        let function_names = ctx
            .function_names_for_statement_idx(statement_idx)
            .cloned()
            .unwrap_or_else(|| vec![FunctionName("test".to_string())]);

        Box::new(code_locations.clone().into_iter().rev().zip(function_names).map(
            |(code_location, function_name)| {
                self.build_stack_frame(&code_location, &function_name, ctx)
            },
        ))
    }

    fn build_stack_frame(
        &self,
        CodeLocation(SourceFileFullPath(source_file), code_span, _): &CodeLocation,
        FunctionName(function_name): &FunctionName,
        ctx: &Context,
    ) -> StackFrame {
        let file_path = Path::new(&source_file);
        let name = function_name.clone();

        let is_user_code = file_path.starts_with(&ctx.root_path);
        let presentation_hint = Some(if is_user_code {
            StackFramePresentationhint::Normal
        } else {
            StackFramePresentationhint::Subtle
        });

        // Annotations from debug info are 0-indexed.
        // UI expects 1-indexed, hence +1 below.
        let line = (code_span.start.line.0 + 1) as i64;
        let column = (code_span.start.col.0 + 1) as i64;

        StackFrame {
            id: self.next_frame_id(),
            name,
            source: Some(Source {
                name: None,
                path: Some(source_file.clone()),
                ..Default::default()
            }),
            line,
            column,
            presentation_hint,
            ..Default::default()
        }
    }

    fn unknown_frame(&self) -> StackFrame {
        StackFrame {
            id: self.next_frame_id(),
            name: "Unknown".to_string(),
            line: 1,
            column: 1,
            presentation_hint: Some(StackFramePresentationhint::Subtle),
            ..Default::default()
        }
    }

    fn next_frame_id(&self) -> i64 {
        MIN_OBJECT_REFERENCE + 2 * self.flat_length() as i64
    }

    fn flat_length(&self) -> usize {
        self.call_frames_and_vars.iter().map(|(frames, _, _)| frames.len()).sum()
    }
}

#[derive(Default, Clone)]
pub struct FunctionVariables {
    pub names_to_values: HashMap<String, String>,
}
