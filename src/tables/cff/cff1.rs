//! A [Compact Font Format Table](
//! https://docs.microsoft.com/en-us/typography/opentype/spec/cff) implementation.

// Useful links:
// http://wwwimages.adobe.com/content/dam/Adobe/en/devnet/font/pdfs/5176.CFF.pdf
// http://wwwimages.adobe.com/content/dam/Adobe/en/devnet/font/pdfs/5177.Type2.pdf
// https://github.com/opentypejs/opentype.js/blob/master/src/tables/cff.js

use core::convert::TryFrom;
use core::ops::Range;

use crate::{GlyphId, OutlineBuilder, Rect, BBox};
use crate::parser::{Stream, LazyArray16, NumFrom, TryNumFrom};
use super::{Builder, IsEven, CFFError, StringId, calc_subroutine_bias, conv_subroutine_index};
use super::argstack::ArgumentsStack;
use super::charset::{STANDARD_ENCODING, Charset, parse_charset};
use super::charstring::CharStringParser;
use super::dict::DictionaryParser;
use super::index::{Index, parse_index, skip_index};
#[cfg(feature = "glyph-names")] use super::std_names::STANDARD_NAMES;

// Limits according to the Adobe Technical Note #5176, chapter 4 DICT Data.
const MAX_OPERANDS_LEN: usize = 48;

// Limits according to the Adobe Technical Note #5177 Appendix B.
const STACK_LIMIT: u8 = 10;
const MAX_ARGUMENTS_STACK_LEN: usize = 48;

const TWO_BYTE_OPERATOR_MARK: u8 = 12;

/// Enumerates some operators defined in the Adobe Technical Note #5177.
mod operator {
    pub const HORIZONTAL_STEM: u8           = 1;
    pub const VERTICAL_STEM: u8             = 3;
    pub const VERTICAL_MOVE_TO: u8          = 4;
    pub const LINE_TO: u8                   = 5;
    pub const HORIZONTAL_LINE_TO: u8        = 6;
    pub const VERTICAL_LINE_TO: u8          = 7;
    pub const CURVE_TO: u8                  = 8;
    pub const CALL_LOCAL_SUBROUTINE: u8     = 10;
    pub const RETURN: u8                    = 11;
    pub const ENDCHAR: u8                   = 14;
    pub const HORIZONTAL_STEM_HINT_MASK: u8 = 18;
    pub const HINT_MASK: u8                 = 19;
    pub const COUNTER_MASK: u8              = 20;
    pub const MOVE_TO: u8                   = 21;
    pub const HORIZONTAL_MOVE_TO: u8        = 22;
    pub const VERTICAL_STEM_HINT_MASK: u8   = 23;
    pub const CURVE_LINE: u8                = 24;
    pub const LINE_CURVE: u8                = 25;
    pub const VV_CURVE_TO: u8               = 26;
    pub const HH_CURVE_TO: u8               = 27;
    pub const SHORT_INT: u8                 = 28;
    pub const CALL_GLOBAL_SUBROUTINE: u8    = 29;
    pub const VH_CURVE_TO: u8               = 30;
    pub const HV_CURVE_TO: u8               = 31;
    pub const HFLEX: u8                     = 34;
    pub const FLEX: u8                      = 35;
    pub const HFLEX1: u8                    = 36;
    pub const FLEX1: u8                     = 37;
    pub const FIXED_16_16: u8               = 255;
}

/// Enumerates some operators defined in the Adobe Technical Note #5176,
/// Table 9 Top DICT Operator Entries
mod top_dict_operator {
    pub const CHARSET_OFFSET: u16               = 15;
    pub const CHAR_STRINGS_OFFSET: u16          = 17;
    pub const PRIVATE_DICT_SIZE_AND_OFFSET: u16 = 18;
    pub const ROS: u16                          = 1230;
    pub const FD_ARRAY: u16                     = 1236;
    pub const FD_SELECT: u16                    = 1237;
}

/// Enumerates some operators defined in the Adobe Technical Note #5176,
/// Table 23 Private DICT Operators
mod private_dict_operator {
    pub const LOCAL_SUBROUTINES_OFFSET: u16 = 19;
}

/// Enumerates Charset IDs defined in the Adobe Technical Note #5176, Table 22
mod charset_id {
    pub const ISO_ADOBE: usize = 0;
    pub const EXPERT: usize = 1;
    pub const EXPERT_SUBSET: usize = 2;
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FontKind<'a> {
    SID(SIDMetadata<'a>),
    CID(CIDMetadata<'a>),
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SIDMetadata<'a> {
    local_subrs: Index<'a>,
}

#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct CIDMetadata<'a> {
    fd_array: Index<'a>,
    fd_select: FDSelect<'a>,
}

#[derive(Default)]
struct TopDict {
    charset_offset: Option<usize>,
    char_strings_offset: usize,
    private_dict_range: Option<Range<usize>>,
    has_ros: bool,
    fd_array_offset: Option<usize>,
    fd_select_offset: Option<usize>,
}

fn parse_top_dict(s: &mut Stream) -> Option<TopDict> {
    let mut top_dict = TopDict::default();

    let index = parse_index::<u16>(s)?;

    // The Top DICT INDEX should have only one dictionary.
    let data = index.get(0)?;

    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        match operator.get() {
            top_dict_operator::CHARSET_OFFSET => {
                top_dict.charset_offset = dict_parser.parse_offset();
            }
            top_dict_operator::CHAR_STRINGS_OFFSET => {
                top_dict.char_strings_offset = dict_parser.parse_offset()?;
            }
            top_dict_operator::PRIVATE_DICT_SIZE_AND_OFFSET => {
                top_dict.private_dict_range = dict_parser.parse_range();
            }
            top_dict_operator::ROS => {
                top_dict.has_ros = true;
            }
            top_dict_operator::FD_ARRAY => {
                top_dict.fd_array_offset = dict_parser.parse_offset();
            }
            top_dict_operator::FD_SELECT => {
                top_dict.fd_select_offset = dict_parser.parse_offset();
            }
            _ => {}
        }
    }

    Some(top_dict)
}

// TODO: move to integration
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dict_size_overflow() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x0C, // index [1]: 14
            0x1D, 0x7F, 0xFF, 0xFF, 0xFF, // length: i32::MAX
            0x1D, 0x7F, 0xFF, 0xFF, 0xFF, // offset: i32::MAX
            0x12 // operator: 18 (private)
        ];

        let top_dict = parse_top_dict(&mut Stream::new(data)).unwrap();
        assert_eq!(top_dict.private_dict_range, Some(2147483647..4294967294));
    }

    #[test]
    fn private_dict_negative_char_strings_offset() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x03, // index [1]: 3
            // Item 0
            0x8A, // offset: -1
            0x11, // operator: 17 (char_string)
        ];

        assert!(parse_top_dict(&mut Stream::new(data)).is_none());
    }

    #[test]
    fn private_dict_no_char_strings_offset_operand() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x02, // index [1]: 2
            // Item 0
            // <-- No number here.
            0x11, // operator: 17 (char_string)
        ];

        assert!(parse_top_dict(&mut Stream::new(data)).is_none());
    }

    #[test]
    fn negative_private_dict_offset_and_size() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x04, // index [1]: 4
            // Item 0
            0x8A, // length: -1
            0x8A, // offset: -1
            0x12, // operator: 18 (private)
        ];

        let top_dict = parse_top_dict(&mut Stream::new(data)).unwrap();
        assert!(top_dict.private_dict_range.is_none());
    }
}

fn parse_private_dict(data: &[u8]) -> Option<usize> {
    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        if operator.get() == private_dict_operator::LOCAL_SUBROUTINES_OFFSET {
            return dict_parser.parse_offset();
        }
    }

    None
}

fn parse_font_dict(data: &[u8]) -> Option<Range<usize>> {
    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        if operator.get() == top_dict_operator::PRIVATE_DICT_SIZE_AND_OFFSET {
            return dict_parser.parse_range();
        }
    }

    None
}

/// In CID fonts, to get local subroutines we have to:
///   1. Find Font DICT index via FDSelect by GID.
///   2. Get Font DICT data from FDArray using this index.
///   3. Get a Private DICT offset from a Font DICT.
///   4. Get a local subroutine offset from Private DICT.
///   5. Parse a local subroutine at offset.
fn parse_cid_local_subrs<'a>(
    data: &'a [u8],
    glyph_id: GlyphId,
    cid: &CIDMetadata,
) -> Option<Index<'a>> {
    let font_dict_index = cid.fd_select.font_dict_index(glyph_id)?;
    let font_dict_data = cid.fd_array.get(u32::from(font_dict_index))?;
    let private_dict_range = parse_font_dict(font_dict_data)?;
    let private_dict_data = data.get(private_dict_range.clone())?;
    let subroutines_offset = parse_private_dict(private_dict_data)?;

    // 'The local subroutines offset is relative to the beginning
    // of the Private DICT data.'
    let start = private_dict_range.start.checked_add(subroutines_offset)?;
    let subrs_data = data.get(start..)?;
    let mut s = Stream::new(subrs_data);
    parse_index::<u16>(&mut s)
}

struct CharStringParserContext<'a> {
    metadata: &'a Table<'a>,
    width_parsed: bool,
    stems_len: u32,
    has_endchar: bool,
    has_seac: bool,
    glyph_id: GlyphId, // Required to parse local subroutine in CID fonts.
    local_subrs: Option<Index<'a>>,
}

fn parse_char_string(
    data: &[u8],
    metadata: &Table,
    glyph_id: GlyphId,
    builder: &mut dyn OutlineBuilder,
) -> Result<Rect, CFFError> {
    let local_subrs = match metadata.kind {
        FontKind::SID(ref sid) => Some(sid.local_subrs),
        FontKind::CID(_) => None, // Will be resolved on request.
    };

    let mut ctx = CharStringParserContext {
        metadata,
        width_parsed: false,
        stems_len: 0,
        has_endchar: false,
        has_seac: false,
        glyph_id,
        local_subrs,
    };

    let mut inner_builder = Builder {
        builder,
        bbox: BBox::new(),
    };

    let stack = ArgumentsStack {
        data: &mut [0.0; MAX_ARGUMENTS_STACK_LEN], // 192B
        len: 0,
        max_len: MAX_ARGUMENTS_STACK_LEN,
    };
    let mut parser = CharStringParser {
        stack,
        builder: &mut inner_builder,
        x: 0.0,
        y: 0.0,
        has_move_to: false,
        is_first_move_to: true,
    };
    _parse_char_string(&mut ctx, data, 0, &mut parser)?;

    if !ctx.has_endchar {
        return Err(CFFError::MissingEndChar);
    }

    let bbox = parser.builder.bbox;

    // Check that bbox was changed.
    if bbox.is_default() {
        return Err(CFFError::ZeroBBox);
    }

    bbox.to_rect().ok_or(CFFError::BboxOverflow)
}


fn _parse_char_string(
    ctx: &mut CharStringParserContext,
    char_string: &[u8],
    depth: u8,
    p: &mut CharStringParser,
) -> Result<(), CFFError> {
    let mut s = Stream::new(char_string);
    while !s.at_end() {
        let op = s.read::<u8>().ok_or(CFFError::ReadOutOfBounds)?;
        match op {
            0 | 2 | 9 | 13 | 15 | 16 | 17 => {
                // Reserved.
                return Err(CFFError::InvalidOperator);
            }
            operator::HORIZONTAL_STEM |
            operator::VERTICAL_STEM |
            operator::HORIZONTAL_STEM_HINT_MASK |
            operator::VERTICAL_STEM_HINT_MASK => {
                // y dy {dya dyb}* hstem
                // x dx {dxa dxb}* vstem
                // y dy {dya dyb}* hstemhm
                // x dx {dxa dxb}* vstemhm

                // If the stack length is uneven, than the first value is a `width`.
                let len = if p.stack.len().is_odd() && !ctx.width_parsed {
                    ctx.width_parsed = true;
                    p.stack.len() - 1
                } else {
                    p.stack.len()
                };

                ctx.stems_len += len as u32 >> 1;

                // We are ignoring the hint operators.
                p.stack.clear();
            }
            operator::VERTICAL_MOVE_TO => {
                let mut i = 0;
                if p.stack.len() == 2 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                }

                p.parse_vertical_move_to(i)?;
            }
            operator::LINE_TO => {
                p.parse_line_to()?;
            }
            operator::HORIZONTAL_LINE_TO => {
                p.parse_horizontal_line_to()?;
            }
            operator::VERTICAL_LINE_TO => {
                p.parse_vertical_line_to()?;
            }
            operator::CURVE_TO => {
                p.parse_curve_to()?;
            }
            operator::CALL_LOCAL_SUBROUTINE => {
                if p.stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if depth == STACK_LIMIT {
                    return Err(CFFError::NestingLimitReached);
                }

                // Parse and remember the local subroutine for the current glyph.
                // Since it's a pretty complex task, we're doing it only when
                // a local subroutine is actually requested by the glyphs charstring.
                if ctx.local_subrs.is_none() {
                    if let FontKind::CID(ref cid) = ctx.metadata.kind {
                        ctx.local_subrs = parse_cid_local_subrs(
                            ctx.metadata.table_data, ctx.glyph_id, cid
                        );
                    }
                }

                if let Some(local_subrs) = ctx.local_subrs {
                    let subroutine_bias = calc_subroutine_bias(local_subrs.len());
                    let index = conv_subroutine_index(p.stack.pop(), subroutine_bias)?;
                    let char_string = local_subrs.get(index).ok_or(CFFError::InvalidSubroutineIndex)?;
                    _parse_char_string(ctx, char_string, depth + 1, p)?;
                } else {
                    return Err(CFFError::NoLocalSubroutines);
                }

                if ctx.has_endchar && !ctx.has_seac {
                    if !s.at_end() {
                        return Err(CFFError::DataAfterEndChar);
                    }

                    break;
                }
            }
            operator::RETURN => {
                break;
            }
            TWO_BYTE_OPERATOR_MARK => {
                // flex
                let op2 = s.read::<u8>().ok_or(CFFError::ReadOutOfBounds)?;
                match op2 {
                    operator::HFLEX => p.parse_hflex()?,
                    operator::FLEX => p.parse_flex()?,
                    operator::HFLEX1 => p.parse_hflex1()?,
                    operator::FLEX1 => p.parse_flex1()?,
                    _ => return Err(CFFError::UnsupportedOperator),
                }
            }
            operator::ENDCHAR => {
                if p.stack.len() == 4 || (!ctx.width_parsed && p.stack.len() == 5) {
                    // Process 'seac'.
                    let accent_char = seac_code_to_glyph_id(&ctx.metadata.charset, p.stack.pop())
                        .ok_or(CFFError::InvalidSeacCode)?;
                    let base_char = seac_code_to_glyph_id(&ctx.metadata.charset, p.stack.pop())
                        .ok_or(CFFError::InvalidSeacCode)?;
                    let dy = p.stack.pop();
                    let dx = p.stack.pop();

                    if !ctx.width_parsed && !p.stack.is_empty() {
                        p.stack.pop();
                        ctx.width_parsed = true;
                    }

                    ctx.has_seac = true;

                    if depth == STACK_LIMIT {
                        return Err(CFFError::NestingLimitReached);
                    }

                    let base_char_string = ctx.metadata.char_strings.get(u32::from(base_char.0))
                        .ok_or(CFFError::InvalidSeacCode)?;
                    _parse_char_string(ctx, base_char_string, depth + 1, p)?;
                    p.x = dx;
                    p.y = dy;

                    let accent_char_string = ctx.metadata.char_strings.get(u32::from(accent_char.0))
                        .ok_or(CFFError::InvalidSeacCode)?;
                    _parse_char_string(ctx, accent_char_string, depth + 1, p)?;
                } else if p.stack.len() == 1 && !ctx.width_parsed {
                    p.stack.pop();
                    ctx.width_parsed = true;
                }

                if !p.is_first_move_to {
                    p.is_first_move_to = true;
                    p.builder.close();
                }

                if !s.at_end() {
                    return Err(CFFError::DataAfterEndChar);
                }

                ctx.has_endchar = true;

                break;
            }
            operator::HINT_MASK | operator::COUNTER_MASK => {
                let mut len = p.stack.len();

                // We are ignoring the hint operators.
                p.stack.clear();

                // If the stack length is uneven, than the first value is a `width`.
                if len.is_odd() && !ctx.width_parsed {
                    len -= 1;
                    ctx.width_parsed = true;
                }

                ctx.stems_len += len as u32 >> 1;

                s.advance(usize::num_from((ctx.stems_len + 7) >> 3));
            }
            operator::MOVE_TO => {
                let mut i = 0;
                if p.stack.len() == 3 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                }

                p.parse_move_to(i)?;
            }
            operator::HORIZONTAL_MOVE_TO => {
                let mut i = 0;
                if p.stack.len() == 2 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                }

                p.parse_horizontal_move_to(i)?;
            }
            operator::CURVE_LINE => {
                p.parse_curve_line()?;
            }
            operator::LINE_CURVE => {
                p.parse_line_curve()?;
            }
            operator::VV_CURVE_TO => {
                p.parse_vv_curve_to()?;
            }
            operator::HH_CURVE_TO => {
                p.parse_hh_curve_to()?;
            }
            operator::SHORT_INT => {
                let n = s.read::<i16>().ok_or(CFFError::ReadOutOfBounds)?;
                p.stack.push(f32::from(n))?;
            }
            operator::CALL_GLOBAL_SUBROUTINE => {
                if p.stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if depth == STACK_LIMIT {
                    return Err(CFFError::NestingLimitReached);
                }

                let subroutine_bias = calc_subroutine_bias(ctx.metadata.global_subrs.len());
                let index = conv_subroutine_index(p.stack.pop(), subroutine_bias)?;
                let char_string = ctx.metadata.global_subrs.get(index)
                    .ok_or(CFFError::InvalidSubroutineIndex)?;
                _parse_char_string(ctx, char_string, depth + 1, p)?;

                if ctx.has_endchar && !ctx.has_seac {
                    if !s.at_end() {
                        return Err(CFFError::DataAfterEndChar);
                    }

                    break;
                }
            }
            operator::VH_CURVE_TO => {
                p.parse_vh_curve_to()?;
            }
            operator::HV_CURVE_TO => {
                p.parse_hv_curve_to()?;
            }
            32..=246 => {
                p.parse_int1(op)?;
            }
            247..=250 => {
                p.parse_int2(op, &mut s)?;
            }
            251..=254 => {
                p.parse_int3(op, &mut s)?;
            }
            operator::FIXED_16_16 => {
                p.parse_fixed(&mut s)?;
            }
        }
    }

    // TODO: 'A charstring subroutine must end with either an endchar or a return operator.'

    Ok(())
}

fn seac_code_to_glyph_id(charset: &Charset, n: f32) -> Option<GlyphId> {
    let code = u8::try_num_from(n)?;

    let sid = STANDARD_ENCODING[code as usize];
    let sid = StringId(u16::from(sid));

    match charset {
        Charset::ISOAdobe => {
            // ISO Adobe charset only defines string ids up to 228 (zcaron)
            if code <= 228 { Some(GlyphId(sid.0)) } else { None }
        }
        Charset::Expert | Charset::ExpertSubset => None,
        _ => charset.sid_to_gid(sid),
    }
}


#[derive(Clone, Copy, Debug)]
enum FDSelect<'a> {
    Format0(LazyArray16<'a, u8>),
    Format3(&'a [u8]), // It's easier to parse it in-place.
}

impl Default for FDSelect<'_> {
    fn default() -> Self {
        FDSelect::Format0(LazyArray16::default())
    }
}

impl FDSelect<'_> {
    fn font_dict_index(&self, glyph_id: GlyphId) -> Option<u8> {
        match self {
            FDSelect::Format0(ref array) => array.get(glyph_id.0),
            FDSelect::Format3(ref data) => {
                let mut s = Stream::new(data);
                let number_of_ranges = s.read::<u16>()?;
                if number_of_ranges == 0 {
                    return None;
                }

                // 'A sentinel GID follows the last range element and serves
                // to delimit the last range in the array.'
                // So we can simply increase the number of ranges by one.
                let number_of_ranges = number_of_ranges.checked_add(1)?;

                // Range is: GlyphId + u8
                let mut prev_first_glyph = s.read::<GlyphId>()?;
                let mut prev_index = s.read::<u8>()?;
                for _ in 1..number_of_ranges {
                    let curr_first_glyph = s.read::<GlyphId>()?;
                    if (prev_first_glyph..curr_first_glyph).contains(&glyph_id) {
                        return Some(prev_index);
                    } else {
                        prev_index = s.read::<u8>()?;
                    }

                    prev_first_glyph = curr_first_glyph;
                }

                None
            }
        }
    }
}

fn parse_fd_select<'a>(number_of_glyphs: u16, s: &mut Stream<'a>) -> Option<FDSelect<'a>> {
    let format = s.read::<u8>()?;
    match format {
        0 => Some(FDSelect::Format0(s.read_array16::<u8>(number_of_glyphs)?)),
        3 => Some(FDSelect::Format3(s.tail()?)),
        _ => None,
    }
}

fn parse_sid_metadata(data: &[u8], top_dict: TopDict) -> Option<FontKind> {
    let subroutines_offset = if let Some(range) = top_dict.private_dict_range.clone() {
        parse_private_dict(data.get(range)?)
    } else {
        None
    };

    // Parse Global Subroutines INDEX.
    let mut metadata = SIDMetadata::default();

    match (top_dict.private_dict_range, subroutines_offset) {
        (Some(private_dict_range), Some(subroutines_offset)) => {
            // 'The local subroutines offset is relative to the beginning
            // of the Private DICT data.'
            if let Some(start) = private_dict_range.start.checked_add(subroutines_offset) {
                let data = data.get(start..data.len())?;
                let mut s = Stream::new(data);
                metadata.local_subrs = parse_index::<u16>(&mut s)?;
            }
        }
        _ => {}
    }

    Some(FontKind::SID(metadata))
}

fn parse_cid_metadata(data: &[u8], top_dict: TopDict, number_of_glyphs: u16) -> Option<FontKind> {
    let (charset_offset, fd_array_offset, fd_select_offset) =
        match (top_dict.charset_offset, top_dict.fd_array_offset, top_dict.fd_select_offset) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return None, // charset, FDArray and FDSelect must be set.
        };

    if charset_offset <= charset_id::EXPERT_SUBSET {
        // 'There are no predefined charsets for CID fonts.'
        // Adobe Technical Note #5176, chapter 18 CID-keyed Fonts
        return None;
    }

    let mut metadata = CIDMetadata::default();

    metadata.fd_array = {
        let mut s = Stream::new_at(data, fd_array_offset)?;
        parse_index::<u16>(&mut s)?
    };

    metadata.fd_select = {
        let mut s = Stream::new_at(data, fd_select_offset)?;
        parse_fd_select(number_of_glyphs, &mut s)?
    };

    Some(FontKind::CID(metadata))
}


/// A [Compact Font Format Table](
/// https://docs.microsoft.com/en-us/typography/opentype/spec/cff).
#[derive(Clone, Copy)]
pub struct Table<'a> {
    // The whole CFF table.
    // Used to resolve a local subroutine in a CID font.
    table_data: &'a [u8],

    #[allow(dead_code)] strings: Index<'a>,
    global_subrs: Index<'a>,
    charset: Charset<'a>,
    char_strings: Index<'a>,
    kind: FontKind<'a>,
}

impl<'a> Table<'a> {
    /// Parses a table from raw data.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let mut s = Stream::new(data);

        // Parse Header.
        let major = s.read::<u8>()?;
        s.skip::<u8>(); // minor
        let header_size = s.read::<u8>()?;
        s.skip::<u8>(); // Absolute offset

        if major != 1 {
            return None;
        }

        // Jump to Name INDEX. It's not necessarily right after the header.
        if header_size > 4 {
            s.advance(usize::from(header_size) - 4);
        }

        // Skip Name INDEX.
        skip_index::<u16>(&mut s)?;

        let top_dict = parse_top_dict(&mut s)?;

        // Must be set, otherwise there are nothing to parse.
        if top_dict.char_strings_offset == 0 {
            return None;
        }

        // String INDEX.
        let strings = parse_index::<u16>(&mut s)?;

        // Parse Global Subroutines INDEX.
        let global_subrs = parse_index::<u16>(&mut s)?;

        let char_strings = {
            let mut s = Stream::new_at(data, top_dict.char_strings_offset)?;
            parse_index::<u16>(&mut s)?
        };

        if char_strings.len() == 0 {
            return None;
        }

        // 'The number of glyphs is the value of the count field in the CharStrings INDEX.'
        let number_of_glyphs = u16::try_from(char_strings.len()).ok()?;

        let charset = match top_dict.charset_offset {
            Some(charset_id::ISO_ADOBE) => Charset::ISOAdobe,
            Some(charset_id::EXPERT) => Charset::Expert,
            Some(charset_id::EXPERT_SUBSET) => Charset::ExpertSubset,
            Some(offset) => parse_charset(number_of_glyphs, &mut Stream::new_at(data, offset)?)?,
            None => Charset::ISOAdobe, // default
        };

        let kind = if top_dict.has_ros {
            parse_cid_metadata(data, top_dict, number_of_glyphs)?
        } else {
            parse_sid_metadata(data, top_dict)?
        };

        Some(Self {
            table_data: data,
            strings,
            global_subrs,
            charset,
            char_strings,
            kind,
        })
    }

    /// Outlines a glyph.
    pub fn outline(
        &self,
        glyph_id: GlyphId,
        builder: &mut dyn OutlineBuilder,
    ) -> Result<Rect, CFFError> {
        let data = self.char_strings.get(u32::from(glyph_id.0)).ok_or(CFFError::NoGlyph)?;
        parse_char_string(data, self, glyph_id, builder)
    }

    /// Returns a glyph name.
    #[cfg(feature = "glyph-names")]
    pub fn glyph_name(&self, glyph_id: GlyphId) -> Option<&'a str> {
        match self.kind {
            FontKind::SID(_) => {
                let sid = self.charset.gid_to_sid(glyph_id)?;
                let sid = usize::from(sid.0);
                match STANDARD_NAMES.get(sid) {
                    Some(name) => Some(name),
                    None => {
                        let idx = u32::try_from(sid - STANDARD_NAMES.len()).ok()?;
                        let name = self.strings.get(idx)?;
                        core::str::from_utf8(name).ok()
                    }
                }
            }
            FontKind::CID(_) => None,
        }
    }
}

impl core::fmt::Debug for Table<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "Table {{ ... }}")
    }
}
