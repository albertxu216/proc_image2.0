// Based on gimli-rs/addr2line (https://github.com/gimli-rs/addr2line):
// > Copyright (c) 2016-2018 The gimli Developers
// >
// > Permission is hereby granted, free of charge, to any
// > person obtaining a copy of this software and associated
// > documentation files (the "Software"), to deal in the
// > Software without restriction, including without
// > limitation the rights to use, copy, modify, merge,
// > publish, distribute, sublicense, and/or sell copies of
// > the Software, and to permit persons to whom the Software
// > is furnished to do so, subject to the following
// > conditions:
// >
// > The above copyright notice and this permission notice
// > shall be included in all copies or substantial portions
// > of the Software.
// >
// > THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// > ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// > TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// > PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// > SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// > CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// > OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// > IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// > DEALINGS IN THE SOFTWARE.

use std::cmp::Ordering;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::vec;

use gimli::Error;

use super::lazy::LazyCell;
use super::range::RangeAttributes;
use super::reader::R;


fn name_entry<R>(
    unit: &gimli::Unit<R>,
    offset: gimli::UnitOffset<R::Offset>,
    sections: &gimli::Dwarf<R>,
    recursion_limit: usize,
) -> Result<Option<R>, Error>
where
    R: gimli::Reader,
{
    let mut entries = unit.entries_raw(Some(offset))?;
    let abbrev = if let Some(abbrev) = entries.read_abbreviation()? {
        abbrev
    } else {
        return Err(gimli::Error::NoEntryAtGivenOffset)
    };

    let mut name = None;
    let mut next = None;
    for spec in abbrev.attributes() {
        match entries.read_attribute(*spec) {
            Ok(ref attr) => match attr.name() {
                gimli::DW_AT_linkage_name | gimli::DW_AT_MIPS_linkage_name => {
                    if let Ok(val) = sections.attr_string(unit, attr.value()) {
                        return Ok(Some(val))
                    }
                }
                gimli::DW_AT_name => {
                    if let Ok(val) = sections.attr_string(unit, attr.value()) {
                        name = Some(val);
                    }
                }
                gimli::DW_AT_abstract_origin | gimli::DW_AT_specification => {
                    next = Some(attr.value());
                }
                _ => {}
            },
            Err(e) => return Err(e),
        }
    }

    if name.is_some() {
        return Ok(name)
    }

    if let Some(next) = next {
        return name_attr(next, unit, sections, recursion_limit - 1)
    }

    Ok(None)
}


fn name_attr<R>(
    attr: gimli::AttributeValue<R>,
    unit: &gimli::Unit<R>,
    sections: &gimli::Dwarf<R>,
    recursion_limit: usize,
) -> Result<Option<R>, Error>
where
    R: gimli::Reader,
{
    if recursion_limit == 0 {
        return Ok(None)
    }

    match attr {
        gimli::AttributeValue::UnitRef(offset) => {
            name_entry(unit, offset, sections, recursion_limit)
        }
        // TODO: Need to handle `AttributeValue::DebugInfoRef` and
        //       `AttributeValue::DebugInfoRefSup`.
        _ => Ok(None),
    }
}


pub(super) struct InlinedFunction<'dwarf> {
    pub(crate) name: Option<R<'dwarf>>,
    pub(crate) call_file: Option<u64>,
    pub(crate) call_line: u32,
    pub(crate) call_column: u32,
}


struct InlinedFunctionAddress {
    range: gimli::Range,
    call_depth: usize,
    /// An index into `Function::inlined_functions`.
    function: usize,
}


pub(super) struct InlinedFunctions<'dwarf> {
    /// List of all `DW_TAG_inlined_subroutine` details in this
    /// function.
    inlined_functions: Box<[InlinedFunction<'dwarf>]>,
    /// List of `DW_TAG_inlined_subroutine` address ranges in this
    /// function.
    inlined_addresses: Box<[InlinedFunctionAddress]>,
}

impl<'dwarf> InlinedFunctions<'dwarf> {
    pub(crate) fn parse(
        dw_die_offset: gimli::UnitOffset<<R<'dwarf> as gimli::Reader>::Offset>,
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
    ) -> Result<Self, Error> {
        let mut entries = unit.entries_raw(Some(dw_die_offset))?;
        let depth = entries.next_depth();
        // The DIE offset we get is the one of the function. But we are
        // interested in its children. So skip the necessary attributes.
        // SANITY: We have parsed the function at the provided offset
        //         earlier.
        let abbrev = entries.read_abbreviation()?.unwrap();
        debug_assert_eq!(abbrev.tag(), gimli::DW_TAG_subprogram);
        let () = entries.skip_attributes(abbrev.attributes())?;

        let mut inlined_functions = Vec::new();
        let mut inlined_addresses = Vec::new();
        Function::parse_children(
            &mut entries,
            depth,
            unit,
            sections,
            &mut inlined_functions,
            &mut inlined_addresses,
            0,
        )?;

        // Sort ranges in "breadth-first traversal order", i.e. first by
        // `call_depth` and then by `range.begin`. This allows finding
        // the range containing an address at a certain depth using
        // binary search.
        // Note: Using DFS order, i.e. ordering by `range.begin` first
        // and then by `call_depth`, would not work!
        // Consider the two examples "[0..10 at depth 0], [0..2 at depth 1],
        // [6..8 at depth 1]" and "[0..5 at depth 0], [0..2 at depth
        // 1], [5..10 at depth 0], [6..8 at depth 1]".
        // In this example, if you want to look up address 7 at depth 0,
        // and you encounter [0..2 at depth 1], are you before or after
        // the target range? You don't know.
        inlined_addresses.sort_by(|r1, r2| {
            (r1.call_depth, r1.range.begin).cmp(&(r2.call_depth, r2.range.begin))
        });

        Ok(Self {
            inlined_functions: inlined_functions.into_boxed_slice(),
            inlined_addresses: inlined_addresses.into_boxed_slice(),
        })
    }

    /// Build the list of inlined functions that contain `probe`.
    pub(super) fn find_inlined_functions(
        &self,
        probe: u64,
    ) -> vec::IntoIter<&InlinedFunction<'dwarf>> {
        // `inlined_functions` is ordered from outside to inside.
        let mut inlined_functions = Vec::new();
        let mut inlined_addresses = &self.inlined_addresses[..];
        loop {
            let current_depth = inlined_functions.len();
            // Look up (probe, current_depth) in inline_ranges.
            // `inlined_addresses` is sorted in "breadth-first traversal order", i.e.
            // by `call_depth` first, and then by `range.begin`. See the comment at
            // the sort call for more information about why.
            let search = inlined_addresses.binary_search_by(|range| {
                if range.call_depth > current_depth {
                    Ordering::Greater
                } else if range.call_depth < current_depth {
                    Ordering::Less
                } else if range.range.begin > probe {
                    Ordering::Greater
                } else if range.range.end <= probe {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            });
            if let Ok(index) = search {
                let function_index = inlined_addresses[index].function;
                inlined_functions.push(&self.inlined_functions[function_index]);
                inlined_addresses = &inlined_addresses[index + 1..];
            } else {
                break
            }
        }
        inlined_functions.into_iter()
    }
}


/// A single address range for a function.
///
/// It is possible for a function to have multiple address ranges; this
/// is handled by having multiple `FunctionAddress` entries with the same
/// `function` field.
#[derive(Debug)]
pub(crate) struct FunctionAddress {
    range: gimli::Range,
    /// An index into `Functions::functions`.
    pub(crate) function: usize,
}


pub(crate) struct Function<'dwarf> {
    pub(crate) dw_die_offset: gimli::UnitOffset<<R<'dwarf> as gimli::Reader>::Offset>,
    /// The function's name, if present.
    pub(crate) name: Option<R<'dwarf>>,
    /// The function's range (begin and end address).
    pub(crate) range: Option<gimli::Range>,
    /// List of inlined function calls.
    pub(super) inlined_functions: LazyCell<Result<InlinedFunctions<'dwarf>, Error>>,
}

impl Debug for Function<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let Self {
            dw_die_offset,
            name,
            range,
            inlined_functions: _,
        } = self;

        f.debug_struct(stringify!(Function))
            .field("dw_die_offset", dw_die_offset)
            .field(
                "name",
                match name.as_ref().and_then(|r| r.to_string().ok()) {
                    Some(ref s) => s,
                    None => &name,
                },
            )
            .field("range", range)
            .finish()
    }
}


#[derive(Debug)]
pub(crate) struct Functions<'dwarf> {
    /// List of all `DW_TAG_subprogram` details in the unit.
    pub(crate) functions: Box<[Function<'dwarf>]>,
    /// List of `DW_TAG_subprogram` address ranges in the unit.
    pub(crate) addresses: Box<[FunctionAddress]>,
}

impl<'dwarf> Functions<'dwarf> {
    pub(crate) fn parse(
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
    ) -> Result<Self, Error> {
        let mut functions = Vec::new();
        let mut addresses = Vec::new();
        let mut entries = unit.entries_raw(None)?;
        while !entries.is_empty() {
            let dw_die_offset = entries.next_offset();
            if let Some(abbrev) = entries.read_abbreviation()? {
                if abbrev.tag() == gimli::DW_TAG_subprogram {
                    let mut name = None;
                    let mut ranges = RangeAttributes::default();
                    for spec in abbrev.attributes() {
                        match entries.read_attribute(*spec) {
                            Ok(ref attr) => {
                                match attr.name() {
                                    gimli::DW_AT_linkage_name | gimli::DW_AT_MIPS_linkage_name => {
                                        if let Ok(val) = sections.attr_string(unit, attr.value()) {
                                            name = Some(val);
                                        }
                                    }
                                    gimli::DW_AT_name => {
                                        if name.is_none() {
                                            name = sections.attr_string(unit, attr.value()).ok();
                                        }
                                    }
                                    gimli::DW_AT_abstract_origin | gimli::DW_AT_specification => {
                                        if name.is_none() {
                                            name = name_attr(attr.value(), unit, sections, 16)?;
                                        }
                                    }
                                    gimli::DW_AT_low_pc => match attr.value() {
                                        gimli::AttributeValue::Addr(val) => {
                                            ranges.low_pc = Some(val)
                                        }
                                        gimli::AttributeValue::DebugAddrIndex(index) => {
                                            ranges.low_pc = Some(sections.address(unit, index)?);
                                        }
                                        _ => {}
                                    },
                                    gimli::DW_AT_high_pc => match attr.value() {
                                        gimli::AttributeValue::Addr(val) => {
                                            ranges.high_pc = Some(val)
                                        }
                                        gimli::AttributeValue::DebugAddrIndex(index) => {
                                            ranges.high_pc = Some(sections.address(unit, index)?);
                                        }
                                        gimli::AttributeValue::Udata(val) => {
                                            ranges.size = Some(val)
                                        }
                                        _ => {}
                                    },
                                    gimli::DW_AT_ranges => {
                                        ranges.ranges_offset =
                                            sections.attr_ranges_offset(unit, attr.value())?;
                                    }
                                    _ => {}
                                };
                            }
                            Err(e) => return Err(e),
                        }
                    }

                    let function_index = functions.len();
                    let added = ranges.for_each_range(sections, unit, |range| {
                        addresses.push(FunctionAddress {
                            range,
                            function: function_index,
                        });
                    })?;

                    if added {
                        let function = Function {
                            dw_die_offset,
                            name,
                            range: ranges.bounds(),
                            inlined_functions: LazyCell::new(),
                        };
                        functions.push(function);
                    }
                } else {
                    entries.skip_attributes(abbrev.attributes())?;
                }
            }
        }

        // The binary search requires the addresses to be sorted.
        //
        // It also requires them to be non-overlapping.  In practice, overlapping
        // function ranges are unlikely, so we don't try to handle that yet.
        //
        // It's possible for multiple functions to have the same address range if the
        // compiler can detect and remove functions with identical code.  In that case
        // we'll nondeterministically return one of them.
        addresses.sort_by_key(|x| x.range.begin);

        Ok(Functions {
            functions: functions.into_boxed_slice(),
            addresses: addresses.into_boxed_slice(),
        })
    }

    #[cfg(test)]
    #[cfg(feature = "nightly")]
    pub(crate) fn parse_inlined_functions(
        &self,
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
    ) -> Result<(), Error> {
        for function in &*self.functions {
            let _inlined_fns = function.parse_inlined_functions(unit, sections)?;
        }
        Ok(())
    }

    pub(crate) fn find_address(&self, probe: u64) -> Option<usize> {
        self.addresses
            .binary_search_by(|address| {
                if probe < address.range.begin {
                    Ordering::Greater
                } else if probe >= address.range.end {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
            .ok()
    }
}

impl<'dwarf> Function<'dwarf> {
    fn parse_children(
        entries: &mut gimli::EntriesRaw<'_, '_, R<'dwarf>>,
        depth: isize,
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
        inlined_functions: &mut Vec<InlinedFunction<'dwarf>>,
        inlined_addresses: &mut Vec<InlinedFunctionAddress>,
        inlined_depth: usize,
    ) -> Result<(), Error> {
        loop {
            let next_depth = entries.next_depth();
            if next_depth <= depth {
                return Ok(())
            }
            if let Some(abbrev) = entries.read_abbreviation()? {
                match abbrev.tag() {
                    gimli::DW_TAG_subprogram => {
                        Function::skip(entries, abbrev, next_depth)?;
                    }
                    gimli::DW_TAG_inlined_subroutine => {
                        InlinedFunction::parse(
                            entries,
                            abbrev,
                            next_depth,
                            unit,
                            sections,
                            inlined_functions,
                            inlined_addresses,
                            inlined_depth,
                        )?;
                    }
                    _ => {
                        entries.skip_attributes(abbrev.attributes())?;
                    }
                }
            }
        }
    }

    pub(super) fn parse_inlined_functions(
        &self,
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
    ) -> Result<&InlinedFunctions<'dwarf>, Error> {
        let inlined_fns = self
            .inlined_functions
            .borrow_with(|| InlinedFunctions::parse(self.dw_die_offset, unit, sections))
            .as_ref()
            .map_err(Error::clone)?;
        Ok(inlined_fns)
    }


    fn skip(
        entries: &mut gimli::EntriesRaw<'_, '_, R<'dwarf>>,
        abbrev: &gimli::Abbreviation,
        depth: isize,
    ) -> Result<(), Error> {
        // TODO: use DW_AT_sibling
        entries.skip_attributes(abbrev.attributes())?;
        while entries.next_depth() > depth {
            if let Some(abbrev) = entries.read_abbreviation()? {
                entries.skip_attributes(abbrev.attributes())?;
            }
        }
        Ok(())
    }
}

impl<'dwarf> InlinedFunction<'dwarf> {
    #[allow(clippy::too_many_arguments)]
    fn parse(
        entries: &mut gimli::EntriesRaw<'_, '_, R<'dwarf>>,
        abbrev: &gimli::Abbreviation,
        depth: isize,
        unit: &gimli::Unit<R<'dwarf>>,
        sections: &gimli::Dwarf<R<'dwarf>>,
        inlined_functions: &mut Vec<InlinedFunction<'dwarf>>,
        inlined_addresses: &mut Vec<InlinedFunctionAddress>,
        inlined_depth: usize,
    ) -> Result<(), Error> {
        let mut ranges = RangeAttributes::default();
        let mut name = None;
        let mut call_file = None;
        let mut call_line = 0;
        let mut call_column = 0;
        for spec in abbrev.attributes() {
            match entries.read_attribute(*spec) {
                Ok(ref attr) => match attr.name() {
                    gimli::DW_AT_low_pc => match attr.value() {
                        gimli::AttributeValue::Addr(val) => ranges.low_pc = Some(val),
                        gimli::AttributeValue::DebugAddrIndex(index) => {
                            ranges.low_pc = Some(sections.address(unit, index)?);
                        }
                        _ => {}
                    },
                    gimli::DW_AT_high_pc => match attr.value() {
                        gimli::AttributeValue::Addr(val) => ranges.high_pc = Some(val),
                        gimli::AttributeValue::DebugAddrIndex(index) => {
                            ranges.high_pc = Some(sections.address(unit, index)?);
                        }
                        gimli::AttributeValue::Udata(val) => ranges.size = Some(val),
                        _ => {}
                    },
                    gimli::DW_AT_ranges => {
                        ranges.ranges_offset = sections.attr_ranges_offset(unit, attr.value())?;
                    }
                    gimli::DW_AT_linkage_name | gimli::DW_AT_MIPS_linkage_name => {
                        if let Ok(val) = sections.attr_string(unit, attr.value()) {
                            name = Some(val);
                        }
                    }
                    gimli::DW_AT_name => {
                        if name.is_none() {
                            name = sections.attr_string(unit, attr.value()).ok();
                        }
                    }
                    gimli::DW_AT_abstract_origin | gimli::DW_AT_specification => {
                        if name.is_none() {
                            name = name_attr(attr.value(), unit, sections, 16)?;
                        }
                    }
                    gimli::DW_AT_call_file => {
                        // There is a spec issue [1] with how DW_AT_call_file is
                        // specified in DWARF 5. Before, a file index of 0 would
                        // indicate no source file, however in DWARF 5 this could
                        // be a valid index into the file table.
                        //
                        // Implementations such as LLVM generates a file index
                        // of 0 when DWARF 5 is used.
                        //
                        // Thus, if we see a version of 5 or later, treat a file
                        // index of 0 as such.
                        // [1]: http://wiki.dwarfstd.org/index.php?title=DWARF5_Line_Table_File_Numbers
                        if let gimli::AttributeValue::FileIndex(fi) = attr.value() {
                            if fi > 0 || unit.header.version() >= 5 {
                                call_file = Some(fi);
                            }
                        }
                    }
                    gimli::DW_AT_call_line => {
                        call_line = attr.udata_value().unwrap_or(0) as u32;
                    }
                    gimli::DW_AT_call_column => {
                        call_column = attr.udata_value().unwrap_or(0) as u32;
                    }
                    _ => {}
                },
                Err(e) => return Err(e),
            }
        }

        let function_index = inlined_functions.len();
        inlined_functions.push(InlinedFunction {
            name,
            call_file,
            call_line,
            call_column,
        });

        ranges.for_each_range(sections, unit, |range| {
            inlined_addresses.push(InlinedFunctionAddress {
                range,
                call_depth: inlined_depth,
                function: function_index,
            });
        })?;

        Function::parse_children(
            entries,
            depth,
            unit,
            sections,
            inlined_functions,
            inlined_addresses,
            inlined_depth + 1,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use test_log::test;


    /// Exercise the `Debug` representation of various types.
    #[test]
    fn debug_repr() {
        let addr = FunctionAddress {
            range: gimli::Range {
                begin: 0x42,
                end: 0x43,
            },
            function: 1337,
        };
        assert_ne!(format!("{addr:?}"), "");

        let func = Function {
            dw_die_offset: gimli::UnitOffset(24),
            name: None,
            range: None,
            inlined_functions: LazyCell::new(),
        };
        assert_ne!(format!("{func:?}"), "");

        let funcs = Functions {
            functions: Box::default(),
            addresses: Box::default(),
        };
        assert_ne!(format!("{funcs:?}"), "");
    }
}
