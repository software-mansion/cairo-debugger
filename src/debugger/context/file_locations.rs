use std::collections::HashMap;
use std::path::PathBuf;

use cairo_annotations::annotations::coverage::{
    CodeLocation, CoverageAnnotationsV1 as SierraCodeLocations,
};
use cairo_lang_sierra::program::StatementIdx;
use cairo_lang_sierra_to_casm::compiler::CairoProgramDebugInfo;

use crate::debugger::context::Context;

/// A map that stores a vector of ***hittable*** Sierra statement indexes for each line in a file.
#[derive(Default)]
pub struct FileCodeLocationsData {
    pub lines: HashMap<Line, Vec<StatementIdx>>,
}

/// Line number in a file, 0-indexed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Default)]
pub struct Line(usize);

impl Line {
    pub fn new(line: usize) -> Self {
        Self(line)
    }

    pub fn create_from_statement_idx(statement_idx: StatementIdx, ctx: &Context) -> Option<Self> {
        let CodeLocation(_, code_span, _) = ctx.code_location_for_statement_idx(statement_idx)?;

        Some(Self(code_span.start.line.0))
    }
}

pub fn build_file_locations_map(
    casm_debug_info: &CairoProgramDebugInfo,
    code_location_annotations: &SierraCodeLocations,
) -> HashMap<PathBuf, FileCodeLocationsData> {
    let mut file_map: HashMap<_, FileCodeLocationsData> = HashMap::new();

    let hittable_statements_code_locations =
        code_location_annotations.statements_code_locations.iter().filter(|(statement_idx, _)| {
            let statement_info = &casm_debug_info.sierra_statement_info[statement_idx.0];

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
            statement_info.start_offset != statement_info.end_offset
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
