use std::collections::HashMap;
use std::fs;
use std::ops::Not;
use std::path::{Path, PathBuf};

use anyhow::{Context as AnyhowContext, Result, anyhow};
use cairo_annotations::annotations::TryFromDebugInfo;
use cairo_annotations::annotations::coverage::{
    CodeLocation, CoverageAnnotationsV1 as SierraCodeLocations, SourceCodeSpan,
};
use cairo_annotations::annotations::debugger::{
    DebuggerAnnotationsV1 as FunctionsDebugInfo, SierraFunctionId, SierraVarId,
};
use cairo_annotations::annotations::profiler::{
    FunctionName, ProfilerAnnotationsV1 as SierraFunctionNames,
};
use cairo_lang_casm::cell_expression::CellExpression;
use cairo_lang_casm::operand::Register;
use cairo_lang_sierra::extensions::core::{CoreConcreteLibfunc, CoreLibfunc, CoreType};
use cairo_lang_sierra::ids::VarId;
use cairo_lang_sierra::program::{Program, ProgramArtifact, Statement, StatementIdx};
use cairo_lang_sierra::program_registry::ProgramRegistry;
use cairo_lang_sierra_to_casm::compiler::{
    CairoProgramDebugInfo, SierraToCasmConfig, StatementKindDebugInfo,
};
use cairo_lang_sierra_to_casm::metadata::calc_metadata;
use cairo_lang_sierra_to_casm::references::ReferenceExpression;
use cairo_vm::Felt252;
use cairo_vm::types::relocatable::MaybeRelocatable;
use cairo_vm::vm::vm_core::VirtualMachine;
use scarb_metadata::MetadataCommand;
use starknet_types_core::felt::Felt;
use tracing::trace;

#[cfg(feature = "dev")]
mod readable_sierra_ids;

/// Struct that holds all the initial data needed for the debugger during execution.
pub struct Context {
    pub root_path: PathBuf,
    code_locations: SierraCodeLocations,
    function_names: SierraFunctionNames,
    files_data: HashMap<PathBuf, FileCodeLocationsData>,
    program: Program,
    sierra_program_registry: ProgramRegistry<CoreType, CoreLibfunc>,
    cairo_var_map: CairoVarMap,
    casm_offsets: CasmDebugInfo,
    #[cfg(feature = "dev")]
    labels: HashMap<usize, String>,
}

type CairoVarMap = HashMap<StatementIdx, HashMap<CairoVar, (VarId, ReferenceExpression)>>;
type CairoVar = (String, SourceCodeSpan);

pub struct CasmDebugInfo {
    /// Sierra statement index -> start CASM bytecode offset
    pub statement_to_pc: Vec<usize>,
}

/// A map that stores a vector of ***hittable*** Sierra statement indexes for each line in a file.
#[derive(Default)]
struct FileCodeLocationsData {
    lines: HashMap<Line, Vec<StatementIdx>>,
}

/// Line number in a file, 0-indexed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub struct Line(usize);

impl Line {
    pub fn new(line: usize) -> Self {
        Self(line)
    }

    pub fn create_from_statement_idx(statement_idx: StatementIdx, ctx: &Context) -> Self {
        let CodeLocation(_, code_span, _) = ctx
            .code_location_for_statement_idx(statement_idx)
            .expect("statement was expected to have corresponding code location");
        Self(code_span.start.line.0)
    }
}

impl Context {
    pub fn new(sierra_path: &Path, casm_offsets: CasmDebugInfo) -> Result<Self> {
        let root_path = get_project_root_path(sierra_path)?;

        let content = fs::read_to_string(sierra_path).expect("Failed to load sierra file");
        let sierra_program: ProgramArtifact = serde_json::from_str(&content)?;
        let program = sierra_program.program;

        let sierra_program_registry =
            ProgramRegistry::new(&program).expect("creating program registry failed");

        let debug_info = sierra_program
            .debug_info
            .ok_or_else(|| anyhow!("debug_info must be present in compiled sierra"))?;

        let code_locations = SierraCodeLocations::try_from_debug_info(&debug_info)?;
        let files_data = build_file_locations_map(&casm_offsets, &code_locations);

        let functions_debug_info = FunctionsDebugInfo::try_from_debug_info(&debug_info)?;

        // Temporary to get casm debug info until it is returned by USC.
        let casm_debug_info = compile_sierra_to_get_casm_debug_info(&program)?;
        let cairo_var_map =
            build_cairo_var_to_casm_map(&program, &casm_debug_info, functions_debug_info);
        eprintln!("{:#?}", cairo_var_map);

        let function_names = SierraFunctionNames::try_from_debug_info(&debug_info)?;

        eprintln!("{}", program);

        Ok(Self {
            #[cfg(feature = "dev")]
            labels: readable_sierra_ids::extract_labels(&program),

            root_path,
            code_locations,
            function_names,
            files_data,
            program,
            sierra_program_registry,
            cairo_var_map,
            casm_offsets,
        })
    }

    pub fn statement_idx_for_pc(&self, pc: usize) -> StatementIdx {
        StatementIdx(
            self.casm_offsets
                .statement_to_pc
                .partition_point(|&offset| offset <= pc)
                .saturating_sub(1),
        )
    }

    /// Return code location for the current statement, not including inlined code locations.
    pub fn code_location_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<CodeLocation> {
        self.code_locations
            .statements_code_locations
            .get(&statement_idx)
            .and_then(|locations| locations.first().cloned())
    }

    /// Return code locations for the current statement, including inlined code locations.
    /// The first element is not inlined.
    pub fn code_locations_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<&Vec<CodeLocation>> {
        self.code_locations.statements_code_locations.get(&statement_idx)
    }

    /// Return function names for the current statement, including inlined function names.
    /// The first element is not inlined.
    pub fn function_names_for_statement_idx(
        &self,
        statement_idx: StatementIdx,
    ) -> Option<&Vec<FunctionName>> {
        self.function_names.statements_functions.get(&statement_idx)
    }

    pub fn statement_idxs_for_breakpoint(
        &self,
        source: &Path,
        line: Line,
    ) -> Option<&Vec<StatementIdx>> {
        self.files_data.get(source)?.lines.get(&line)
    }

    pub fn is_return_statement(&self, statement_idx: StatementIdx) -> bool {
        matches!(self.statement_idx_to_statement(statement_idx), Statement::Return(_))
    }

    pub fn is_function_call_statement(&self, statement_idx: StatementIdx) -> bool {
        match self.statement_idx_to_statement(statement_idx) {
            Statement::Invocation(invocation) => {
                matches!(
                    self.sierra_program_registry.get_libfunc(&invocation.libfunc_id),
                    Ok(CoreConcreteLibfunc::FunctionCall(_))
                )
            }
            Statement::Return(_) => false,
        }
    }

    fn statement_idx_to_statement(&self, statement_idx: StatementIdx) -> &Statement {
        &self.program.statements[statement_idx.0]
    }

    #[cfg(feature = "dev")]
    #[allow(unused)]
    pub fn print_statement(&self, statement_idx: StatementIdx) {
        let statement = self.statement_idx_to_statement(statement_idx);
        let with_labels = readable_sierra_ids::replace_statement_id(statement.clone(), |idx| {
            self.labels[&idx.0].clone()
        });

        eprintln!("{with_labels}")
    }

    pub fn get_values_of_variables(
        &self,
        current_statement_idx: StatementIdx,
        vm: &VirtualMachine,
    ) -> HashMap<String, String> {
        // TODO: Use previous idx from THIS function to determine what statements need to be checked.
        //  Maybe store it in the call stack?
        //  For now we always analyze the function from the beginning to current statement.

        // TODO: The approach described above may be tricky for recursive functions (e.g. loops).
        //  Check the length of a stack in such case.
        let function_entrypoint = &self.program.funcs[self
            .program
            .funcs
            .partition_point(|x| x.entry_point.0 <= current_statement_idx.0)
            - 1]
        .entry_point;

        let mut current_var_values: HashMap<String, (SourceCodeSpan, VarId, Vec<Felt252>)> =
            HashMap::new();

        for idx in function_entrypoint.0..=current_statement_idx.0 {
            let Some(variables) = self.cairo_var_map.get(&StatementIdx(idx)) else { continue };

            for ((name, span), (var_id, ref_expr)) in variables {
                let cells = &ref_expr.cells;
                let mut cells_vals = vec![];
                for cell in cells {
                    match cell {
                        CellExpression::Deref(cell_ref) => {
                            let mut relocatable = match cell_ref.register {
                                Register::AP => vm.get_ap(),
                                Register::FP => vm.get_fp(),
                            };
                            let offset_from_register = cell_ref.offset as isize;
                            let register_offset = relocatable.offset as isize;
                            relocatable.offset =
                                (register_offset + offset_from_register).try_into().unwrap();

                            match vm.segments.memory.get_maybe_relocatable(relocatable) {
                                Ok(MaybeRelocatable::Int(value)) => cells_vals.push(value),
                                Ok(MaybeRelocatable::RelocatableValue(relocatable)) => {
                                    trace!("UNEXPECTED RELOCATABLE (MAYBE ARRAY): {relocatable:?}")
                                }
                                Err(_) => (),
                            }
                        }
                        CellExpression::DoubleDeref(..) => {
                            trace!("DOUBLE Ds")
                        }
                        CellExpression::Immediate(value) => cells_vals.push(Felt::from(value)),
                        CellExpression::BinOp { .. } => {
                            trace!("BINOP")
                        }
                    };
                }

                if let Some((curr_span, _, _)) = current_var_values.get(name) {
                    // If there is a var with the same name in the map already,
                    // and it is further in the code, ignore the current var.
                    if span.start.line < curr_span.start.line
                        || (span.start.line == curr_span.start.line
                            && span.start.col < curr_span.start.col)
                    {
                        continue;
                    }
                }

                if cells_vals.is_empty() {
                    continue;
                }

                current_var_values.insert(name.clone(), (span.clone(), var_id.clone(), cells_vals));
            }

            // // Marks consumed vars as invalid in case of an invocation.
            // match &self.program.statements[idx] {
            //     Statement::Invocation(invocation) => {
            //         match self.sierra_program_registry.get_libfunc(&invocation.libfunc_id).unwrap()
            //         {
            //             // Ignore `drop` since it does not consume the var at Cairo level.
            //             CoreConcreteLibfunc::Drop(_) => {}
            //             _ => {
            //                 let dummy = Vec::new();
            //                 let vars_to_preserve = if let [
            //                     GenBranchInfo { target: GenBranchTarget::Fallthrough, results },
            //                 ] = invocation.branches.as_slice()
            //                 {
            //                     results
            //                 } else {
            //                     &dummy
            //                 };
            //
            //                 for var_id in
            //                     invocation.args.iter().filter(|var| !vars_to_preserve.contains(var))
            //                 {
            //                     if let Some(name_to_remove) = current_var_values
            //                         .iter()
            //                         .find(|(_, (_, id, _))| id.id == var_id.id)
            //                         .map(|(name, _)| name.clone())
            //                     {
            //                         current_var_values.remove(&name_to_remove);
            //                     }
            //                 }
            //             }
            //         }
            //     }
            //     Statement::Return(_) => {}
            // }
        }

        #[cfg(feature = "dev")]
        self.print_statement(current_statement_idx);
        // eprintln!("{current_statement_idx:?}: {current_var_values:#?}");
        eprintln!();

        current_var_values
            .into_iter()
            .filter_map(|(name, (loc, var_id, value_in_felts))| {
                if value_in_felts.len() == 1 {
                    Some((name, value_in_felts[0].to_string()))
                } else {
                    eprintln!("UNSUPPORTED VALUE: ({name}, {loc:?}) {var_id:?} {value_in_felts:?}");
                    None
                }
            })
            .collect()
    }
}

fn build_file_locations_map(
    casm_debug_info: &CasmDebugInfo,
    code_location_annotations: &SierraCodeLocations,
) -> HashMap<PathBuf, FileCodeLocationsData> {
    let mut file_map: HashMap<_, FileCodeLocationsData> = HashMap::new();

    let hittable_statements_code_locations =
        code_location_annotations.statements_code_locations.iter().filter(|(statement_idx, _)| {
            let statement_offset = casm_debug_info.statement_to_pc[statement_idx.0];
            let next_statement_offset = casm_debug_info.statement_to_pc.get(statement_idx.0 + 1);

            // If the next sierra statement maps to the same pc, it means the compilation of the
            // current statement did not produce any CASM instructions.
            // Because of that there is no actual pc that corresponds to such a statement -
            // and therefore the statement is not hittable.
            //
            // An example:
            // ```
            // fn main() -> felt252 {
            //   let x = 5;
            //   let y = @x; // <- The Line
            //   x + 5
            // }
            // The Line compiles to (with optimizations turned off during Cairo->Sierra compilation)
            // to a statement `snapshot_take<felt252>([0]) -> ([1], [2]);. This libfunc takes
            // a sierra variable of id 0 and returns its original value and its duplicate, which are
            // now "in" sierra vars of id 1 and 2.
            // Even though the statement maps to some Cairo code in coverage mappings,
            // it does not compile to any CASM instructions directly - check the link below.
            // https://github.com/starkware-libs/cairo/blob/27f9d1a3fcd00993ff43016ce9579e36064e5266/crates/cairo-lang-sierra-to-casm/src/invocations/mod.rs#L718
            // TODO(#61): compare `start_offset` and `end_offset` of current statement instead once USC
            //  (and thus snforge) starts providing full `CairoProgramDebugInfo` + update the comment.
            next_statement_offset.is_some_and(|offset| *offset == statement_offset).not()
        });

    for (statement_idx, locations) in hittable_statements_code_locations {
        // Take only the non-inlined location into the account - the rest of them are not hittable.
        if let Some(loc) = locations.first() {
            let path_str = &loc.0.0;
            let path = PathBuf::from(path_str);

            let start_location = &loc.1.start;
            let line = Line::new(start_location.line.0);

            file_map.entry(path).or_default().lines.entry(line).or_default().push(*statement_idx);
        }
    }

    file_map
}

fn build_cairo_var_to_casm_map(
    program: &Program,
    cairo_program_debug_info: &CairoProgramDebugInfo,
    functions_debug_info: FunctionsDebugInfo,
) -> CairoVarMap {
    let mut cairo_var_to_ref_expr = HashMap::new();
    for (idx, statement_debug_info) in
        cairo_program_debug_info.sierra_statement_info.iter().enumerate()
    {
        let (casm_ref_expressions_for_vars, vars) =
            match (&program.statements[idx], &statement_debug_info.additional_kind_info) {
                (
                    Statement::Invocation(invocation),
                    StatementKindDebugInfo::Invoke(invocation_debug),
                ) => {
                    let casm_ref_expressions_for_vars: Vec<_> =
                        invocation_debug.ref_values.iter().cloned().map(|x| x.expression).collect();
                    (casm_ref_expressions_for_vars, invocation.args.clone())
                }
                (Statement::Return(vars), StatementKindDebugInfo::Return(return_debug)) => {
                    let casm_ref_expressions_for_vars: Vec<_> =
                        return_debug.ref_values.iter().cloned().map(|x| x.expression).collect();
                    (casm_ref_expressions_for_vars, vars.clone())
                }
                _ => unreachable!(),
            };

        assert_eq!(casm_ref_expressions_for_vars.len(), vars.len());
        let function_id =
            &program.funcs[program.funcs.partition_point(|x| x.entry_point.0 <= idx) - 1].id;
        let func_debug_info =
            &functions_debug_info.functions_info[&SierraFunctionId(function_id.id)];

        for (casm_expressions, var_id) in casm_ref_expressions_for_vars.iter().zip(vars.clone()) {
            let Some((name, span)) =
                func_debug_info.sierra_to_cairo_variable.get(&SierraVarId(var_id.id))
            else {
                continue;
            };
            cairo_var_to_ref_expr
                .entry(StatementIdx(idx))
                .or_insert_with(HashMap::new)
                .insert((name.clone(), span.clone()), (var_id, casm_expressions.clone()));
        }
    }

    cairo_var_to_ref_expr
}

fn compile_sierra_to_get_casm_debug_info(program: &Program) -> Result<CairoProgramDebugInfo> {
    let metadata = calc_metadata(program, Default::default())
        .with_context(|| "Failed calculating metadata.")?;
    let cairo_program = cairo_lang_sierra_to_casm::compiler::compile(
        program,
        &metadata,
        SierraToCasmConfig { gas_usage_check: true, max_bytecode_size: usize::MAX },
    )
    .with_context(|| "Compilation failed.")?;

    Ok(cairo_program.debug_info)
}

// TODO(#50)
fn get_project_root_path(sierra_path: &Path) -> Result<PathBuf> {
    Ok(MetadataCommand::new()
        .current_dir(sierra_path.parent().expect("Compiled Sierra must be in target directory"))
        .inherit_stderr()
        .exec()
        .context("Failed to get project metadata from Scarb")?
        .workspace
        .root
        .into())
}
