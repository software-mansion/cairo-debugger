use std::collections::HashMap;
use std::ops::Not;
use std::path::PathBuf;

use cairo_annotations::annotations::coverage::{
    CodeLocation, CoverageAnnotationsV1 as SierraCodeLocations,
};
use cairo_lang_sierra::program::StatementIdx;

use crate::debugger::context::{Context, StatementsStartOffsets};

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

    pub fn create_from_statement_idx(statement_idx: StatementIdx, ctx: &Context) -> Self {
        let CodeLocation(_, code_span, _) = ctx
            .code_location_for_statement_idx(statement_idx)
            .expect("statement was expected to have corresponding code location");
        Self(code_span.start.line.0)
    }
}

pub fn build_file_locations_map(
    statements_start_offsets: &StatementsStartOffsets,
    code_location_annotations: &SierraCodeLocations,
) -> HashMap<PathBuf, FileCodeLocationsData> {
    let mut file_map: HashMap<_, FileCodeLocationsData> = HashMap::new();

    let hittable_statements_code_locations =
        code_location_annotations.statements_code_locations.iter().filter(|(statement_idx, _)| {
            let statement_offset = statements_start_offsets.statement_to_pc[statement_idx.0];
            let next_statement_offset =
                statements_start_offsets.statement_to_pc.get(statement_idx.0 + 1);

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
