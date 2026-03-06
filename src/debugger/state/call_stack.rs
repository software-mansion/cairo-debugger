use std::iter;
use std::path::Path;

use cairo_annotations::annotations::coverage::{CodeLocation, SourceFileFullPath};
use cairo_annotations::annotations::profiler::FunctionName;
use cairo_lang_casm::operand::{CellRef, Register};
use cairo_lang_sierra::program::{BranchTarget, GenBranchTarget, StatementIdx};
use cairo_vm::types::relocatable::Relocatable;
use cairo_vm::vm::vm_core::VirtualMachine;
use dap::types::{Scope, ScopePresentationhint, StackFrame, Variable};
use dap::types::{Source, StackFramePresentationhint};
use indexmap::IndexMap;

use crate::debugger::MIN_OBJECT_REFERENCE;
use crate::debugger::context::Context;
use crate::debugger::state::call_stack::variables::get_values_of_variables;

mod variables;

type SubStack = Vec<(StackFrame, FunctionVariables)>;

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
    /// The stack also contains [`SierraFunctionContext`] corresponding to the function frame.
    /// It is done to ensure each Sierra function frame has its own instance of the context.
    ///
    /// [Object references](https://microsoft.github.io/debug-adapter-protocol/overview#lifetime-of-objects-references):
    /// object reference for each stack frame is equal to its `1 + 2 * flat_index`
    /// where `flat_index` is its position (0-indexed) in the flattened vector (vector of tuples).
    /// For the variables' scope, the object reference is equal to `2 + 2 * flat_index`.
    call_frames_and_vars: Vec<(SubStack, SierraFunctionContext)>,

    /// The context of the currently executed ***Sierra*** function.
    current_sierra_function_context: SierraFunctionContext,

    /// Modification that should be applied to the stack when a new sierra statement is reached.
    ///
    /// This field is there to ensure that a correct stack trace is returned when a current
    /// statement maps to a function call or a return statement.
    /// The stack should be modified ***after*** such a statement is executed.
    action_on_new_statement: Option<Action>,
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
                let sierra_function_context =
                    std::mem::take(&mut self.current_sierra_function_context);

                self.call_frames_and_vars.push((frames_and_variables, sierra_function_context));
            }
            Some(Action::Pop) => {
                if let Some((_, sierra_function_context)) = self.call_frames_and_vars.pop() {
                    self.current_sierra_function_context = sierra_function_context;
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
        // This should never happen, but better safe than sorry.
        if Some(statement_idx) == self.current_sierra_function_context.last_executed_statement {
            return;
        }

        self.current_sierra_function_context.handle_branch_entrances(statement_idx, ctx, vm);
        self.current_sierra_function_context.last_executed_statement = Some(statement_idx);

        if ctx.is_function_call_statement(statement_idx) {
            let frames: Vec<_> = self.build_stack_frames(ctx, statement_idx).collect();
            // TODO(#95): handle variables of inlined functions.
            let vars = iter::repeat_n(FunctionVariables::default(), frames.len() - 1).chain(
                iter::once(get_values_of_variables(
                    ctx,
                    vm,
                    &self.current_sierra_function_context.post_statements_registers,
                )),
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
        requested_variables: RequestedVariables,
        ctx: &Context,
        vm: &VirtualMachine,
    ) -> Vec<Variable> {
        let flat_index = match requested_variables {
            RequestedVariables::CurrentFunction => self.flat_length(),
            RequestedVariables::VariablesReference(variables_reference) => {
                (variables_reference / 2 - 1) as usize
            }
        };

        let FunctionVariables { names_to_values } = if flat_index >= self.flat_length() {
            get_values_of_variables(
                ctx,
                vm,
                &self.current_sierra_function_context.post_statements_registers,
            )
        } else {
            self.call_frames_and_vars
                .iter()
                .flat_map(|substack| &substack.0)
                .map(|(_, vars)| vars)
                .nth(flat_index)
                .unwrap()
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
        self.call_frames_and_vars.iter().map(|(frames, _)| frames.len()).sum()
    }
}

type PostStatementsRegisters =
    IndexMap<StatementIdx, (GenBranchTarget<StatementIdx>, RegistersValues)>;

#[derive(Default)]
struct SierraFunctionContext {
    /// Mapping from sierra statements executed during the function execution to values of registers
    /// right after entering any branch of the statement (by definition only one branch is entered)
    /// and the entered branch target.
    ///
    ///
    /// Note that in general this is ***NOT*** the same as values of registers right after executing
    /// the statement itself, even though it may be the case for a lot of simple libfuncs.
    ///
    /// The mapping uses [`IndexMap`] to memorise the order of statement execution, allowing for
    /// retrieval of the trace of executed sierra statements within the function.
    post_statements_registers: PostStatementsRegisters,

    last_executed_statement: Option<StatementIdx>,
}

impl SierraFunctionContext {
    fn handle_branch_entrances(
        &mut self,
        statement_idx: StatementIdx,
        ctx: &Context,
        vm: &VirtualMachine,
    ) {
        let registers_values = RegistersValues { ap: vm.get_ap().offset, fp: vm.get_fp().offset };

        let Some(previous_statement_idx) = &self.last_executed_statement else {
            // If it is the first statement in this frame, all statements from the function
            // entrypoint to this statement:
            // 1. Have not compiled to any CASM instructions.
            // 2. Would have been executed if the execution happened via sierra executor.
            let entry_point = ctx.sierra_function_for_statement(statement_idx).entry_point;
            let executed_zero_casm_statements = (entry_point.0..statement_idx.0).map(|idx| {
                (
                    StatementIdx(idx),
                    (
                        // All statements compiling to no CASM have only a fallthrough branch.
                        BranchTarget::Fallthrough,
                        registers_values.clone(),
                    ),
                )
            });

            self.post_statements_registers.extend(executed_zero_casm_statements);
            return;
        };

        // We had to hit one a branch of the last executed statement in this function.
        // To find which one, find a path that goes from any branch entrance to `statement_idx`,
        // but only through statements that compile to no CASM.
        // Such statements they are the only ones that could have been "executed"
        // between the current statement and the lastly executed one.
        let hit_branch_entrance = ctx
            .branches_for_statement(*previous_statement_idx)
            .into_iter()
            // We never encountered a case where there is more than one such path.
            // If there exists a case like this, we cannot know which one was taken anyway,
            // so we take the first one.
            .find(|entrance| {
                let mut current = *entrance;
                while current != statement_idx && !ctx.does_compile_to_casm(current) {
                    // All statements compiling to no CASM have only a fallthrough branch.
                    current.0 += 1;
                }

                current == statement_idx
            })
            // There has to be at least one path like this.
            .unwrap();

        let hit_branch_target = if hit_branch_entrance.0 == previous_statement_idx.0 + 1 {
            GenBranchTarget::Fallthrough
        } else {
            GenBranchTarget::Statement(hit_branch_entrance)
        };

        let executed_branch_hit =
            (*previous_statement_idx, (hit_branch_target, registers_values.clone()));
        let executed_zero_casm_statements =
            ((hit_branch_entrance.0 + 1)..statement_idx.0).map(|idx| {
                (
                    StatementIdx(idx),
                    (
                        // All statements compiling to no CASM have only a fallthrough branch.
                        BranchTarget::Fallthrough,
                        registers_values.clone(),
                    ),
                )
            });
        let executed_statements =
            iter::once(executed_branch_hit).chain(executed_zero_casm_statements);

        self.post_statements_registers.extend(executed_statements);
    }
}

enum Action {
    Push(SubStack),
    Pop,
}

#[derive(Default, Clone)]
struct FunctionVariables {
    names_to_values: IndexMap<String, String>,
}

pub enum RequestedVariables {
    CurrentFunction,
    VariablesReference(i64),
}

#[derive(Clone, Debug)]
struct RegistersValues {
    ap: usize,
    fp: usize,
}

impl RegistersValues {
    pub fn relocatable_from_cell_ref(&self, cell_ref: &CellRef) -> Relocatable {
        let original_offset = match cell_ref.register {
            Register::AP => self.ap,
            Register::FP => self.fp,
        };
        let offset = (original_offset as isize + cell_ref.offset as isize).try_into().unwrap();

        // Segment index is always one for ap and fp.
        // https://web.archive.org/web/20240228050216/http://docs.cairo-lang.org/how_cairo_works/segments.html
        Relocatable { segment_index: 1, offset }
    }
}
