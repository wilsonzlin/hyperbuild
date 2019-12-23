use crate::proc::{Processor, Match};
use crate::proc::attr::AttrType;
use crate::code::Code;
use crate::spec::codepoint::is_whitespace;
use crate::proc::entity::{process_entity, parse_entity};
use crate::err::HbRes;
use phf::Map;
use std::thread::current;

pub fn is_double_quote(c: u8) -> bool {
    c == b'"'
}

pub fn is_single_quote(c: u8) -> bool {
    c == b'\''
}

// Valid attribute quote characters.
// See https://html.spec.whatwg.org/multipage/introduction.html#intro-early-example for spec.
pub fn is_attr_quote(c: u8) -> bool {
    // Backtick is not a valid quote character according to spec.
    is_double_quote(c) || is_single_quote(c)
}

pub fn is_unquoted_delimiter(c: u8) -> bool {
    is_whitespace(c) || c == b'>'
}

static ENCODED: Map<u8, &'static [u8]> = phf_map! {
    b'\'' => b"&#39;",
    b'"' => b"&#34;",
    b'>' => b"&gt;",
    // Whitespace characters as defined by spec in crate::spec::codepoint::is_whitespace.
    0x09 => b"&#9;",
    0x0a => b"&#10;",
    0x0c => b"&#12;",
    0x0d => b"&#13;",
    0x20 => b"&#32;",
};

#[derive(Clone, Copy)]
enum CharType {
    End,
    MalformedEntity,
    DecodedNonAscii,
    // Normal needs associated character to be able to write it.
    Normal(u8),
    // Whitespace needs associated character to determine cost of encoding it.
    Whitespace(u8),
    SingleQuote,
    DoubleQuote,
    RightChevron,
}

impl CharType {
    fn from_char(c: u8) -> CharType {
        match c {
            b'"' => CharType::DoubleQuote,
            b'\'' => CharType::SingleQuote,
            b'>' => CharType::RightChevron,
            c => if is_whitespace(c) { CharType::Whitespace(c) } else { CharType::Normal },
        }
    }
}

#[derive(Clone, Copy)]
enum DelimiterType {
    Double,
    Single,
    Unquoted,
}

struct Metrics {
    count_double_quotation: usize,
    count_single_quotation: usize,
    // NOTE: This count is amount after any trimming and collapsing of whitespace.
    count_whitespace: usize,
    // Since whitespace characters have varying encoded lengths, also calculate total length if all of them had to be encoded.
    total_whitespace_encoded_length: usize,
    // First and last character value types after any trimming and collapsing of whitespace.
    // NOTE: First/last value characters, not quotes/delimiters.
    first_char_type: Option<CharType>,
    last_char_type: Option<CharType>,
    // How many times `collect_char_type` is called. Used to determine first and last characters when writing.
    collected_count: usize,
}

impl Metrics {
    // Update metrics with next character type.
    fn collect_char_type(&mut self, char_type: CharType) -> () {
        match char_type {
            CharType::Whitespace(c) => {
                self.count_whitespace += 1;
                self.total_whitespace_encoded_length += ENCODED[c].len();
            }
            CharType::SingleQuote => self.count_single_quotation += 1,
            CharType::DoubleQuote => self.count_double_quotation += 1,
            _ => (),
        };

        if self.first_char_type == None {
            self.first_char_type = Some(char_type);
        };
        self.last_char_type = Some(char_type);
        self.collected_count += 1;
    }

    fn unquoted_cost(&self) -> usize {
        // Costs for encoding first and last characters if going with unquoted attribute value.
        // NOTE: Don't need to consider whitespace for either as all whitespace will be encoded and counts as part of `total_whitespace_encoded_length`.
        let first_char_encoding_cost = match self.first_char_type {
            // WARNING: Change `first_char_is_quote_encoded` if changing here.
            Some(CharType::DoubleQuote) => ENCODED[b'"'].len(),
            Some(CharType::SingleQuote) => ENCODED[b'\''].len(),
            _ => 0,
        };
        let first_char_is_quote_encoded = first_char_encoding_cost > 0;
        let last_char_encoding_cost = match last_char_type {
            Some(CharType::RightChevron) => ENCODED[b'>'].len(),
            _ => 0,
        };

        first_char_encoding_cost
            + self.count_double_quotation
            + self.count_single_quotation
            + self.total_whitespace_encoded_length
            + last_char_encoding_cost
            // If first char is quote and is encoded, it will be counted twice as it'll also be part of `metrics.count_*_quotation`.
            // Subtract last to prevent underflow.
            - first_char_is_quote_encoded as usize
    }

    fn single_quoted_cost(&self) -> usize {
        self.count_single_quotation * ENCODED[b'\''].len() + self.count_double_quotation + self.count_whitespace
    }

    fn double_quoted_cost(&self) -> usize {
        self.count_double_quotation * ENCODED[b'"'].len() + self.count_single_quotation + self.count_whitespace
    }

    fn get_optimal_delimiter_type(&self) -> DelimiterType {
        // When all equal, prefer double quotes to all and single quotes to unquoted.
        let mut min = (DelimiterType::Double, self.double_quoted_cost());

        let single = (DelimiterType::Single, self.single_quoted_cost());
        if single.1 < min.1 {
            min = single;
        };

        let unquoted = (DelimiterType::Unquoted, self.unquoted_cost());
        if unquoted.1 < min.1 {
            min = unquoted;
        };

        min.0
    }
}

fn consume_attr_value<D: Code>(
    proc: &Processor<D>,
    should_collapse_and_trim_ws: bool,
    delimiter_pred: fn(u8) -> bool,
    on_entity: fn(&Processor<D>) -> HbRes<Option<u32>>,
    on_char: fn(char_type: CharType, char_no: usize) -> (),
) -> HbRes<()> {
    // Set to true when one or more immediately previous characters were whitespace and deferred for processing after the contiguous whitespace.
    // NOTE: Only used if `should_collapse_and_trim_ws`.
    let mut currently_in_whitespace = false;
    let mut char_no = 0;
    loop {
        let char_type = if proc.match_pred(delimiter_pred).matched() {
            // DO NOT BREAK HERE. More processing is done afterwards upon reaching end.
            CharType::End
        } else if proc.match_char(b'&').matched() {
            match on_entity(proc)? {
                Some(e) => if e <= 0x7f { CharType::from_char(e as u8) } else { CharType::DecodedNonAscii },
                None => CharType::MalformedEntity,
            }
        } else {
            CharType::from_char(proc.skip()?)
        };

        if should_collapse_and_trim_ws {
            if let CharType::Whitespace(_) = char_type {
                // Ignore this whitespace character, but mark the fact that we are currently in contiguous whitespace.
                currently_in_whitespace = true;
                continue;
            } else {
                // Now past whitespace (e.g. moved to non-whitespace char or end of attribute value). Either:
                // - ignore contiguous whitespace (i.e. do nothing) if we are currently at beginning or end of value; or
                // - collapse contiguous whitespace (i.e. count as one whitespace char) otherwise.
                if currently_in_whitespace && first_char_type != None && char_type != CharType::End {
                    // Collect current collapsed contiguous whitespace that was ignored previously.
                    on_char(CharType::Whitespace(b' '), char_no);
                    char_no += 1;
                };
                currently_in_whitespace = false;
            };
        };

        if char_type == CharType::End {
            break;
        } else {
            on_char(char_type, char_no);
            char_no += 1;
        };
    };

    Ok(())
}

// TODO Might encounter danger if Unicode whitespace is considered as whitespace.
pub fn process_quoted_val<D: Code>(proc: &Processor<D>, should_collapse_and_trim_ws: bool) -> HbRes<AttrType> {
    // Processing a quoted attribute value is tricky, due to the fact that
    // it's not possible to know whether or not to unquote the value until
    // the value has been processed. For example, decoding an entity could
    // create whitespace in a value which might otherwise be unquotable. How
    // this function works is:
    //
    // 1. Assume that the value is unquotable, and don't output any quotes.
    // Decode any entities as necessary. Collect metrics on the types of
    // characters in the value while processing.
    // 2. Based on the metrics, if it's possible to not use quotes, nothing
    // needs to be done and the function ends.
    // 3. Choose a quote based on the amount of occurrences, to minimise the
    // amount of encoded values.
    // 4. Post-process the output by adding delimiter quotes and encoding
    // quotes in values. This does mean that the output is written to twice.

    let src_delimiter = proc.match_pred(is_attr_quote).discard().maybe_char();
    let src_delimiter_pred = match src_delimiter {
        Some(b'"') => is_double_quote,
        Some(b'\'') => is_single_quote,
        None => is_unquoted_delimiter,
        _ => unreachable!(),
    };

    // Stage 1: read and collect metrics on attribute value characters.
    let value_start_checkpoint = proc.checkpoint();
    let mut metrics = Metrics {
        count_double_quotation: 0,
        count_single_quotation: 0,
        count_whitespace: 0,
        total_whitespace_encoded_length: 0,
        first_char_type: None,
        last_char_type: None,
        collected_count: 0,
    };
    consume_attr_value(
        proc,
        should_collapse_and_trim_ws,
        src_delimiter_pred,
        parse_entity,
        |char_type, _| metrics.collect_char_type(char_type),
    )?;

    // Stage 2: optimally minify attribute value using metrics.
    value_start_checkpoint.restore();
    let optimal_delimiter = metrics.get_optimal_delimiter_type();
    let optimal_delimiter_char = match optimal_delimiter {
        DelimiterType::Double => Some(b'"'),
        DelimiterType::Single => Some(b'\''),
        _ => None,
    };
    // Write opening delimiter, if any.
    if let Some(c) = optimal_delimiter_char {
        proc.write(c);
    }
    consume_attr_value(
        proc,
        should_collapse_and_trim_ws,
        src_delimiter_pred,
        process_entity,
        |char_type, char_no| match char_type {
            // This should never happen.
            CharType::End => unreachable!(),

            // Ignore these; already written by process_entity.
            CharType::MalformedEntity => {}
            CharType::DecodedNonAscii => {}

            CharType::Normal(c) => proc.write(c),
            // If unquoted, encode any whitespace anywhere.
            CharType::Whitespace(c) => match optimal_delimiter {
                DelimiterType::Unquoted => proc.write(ENCODED[c]),
                _ => proc.write(c),
            },
            // If single quoted, encode any single quote anywhere.
            // If unquoted, encode single quote if first character.
            CharType::SingleQuote => match (optimal_delimiter, char_no) {
                (DelimiterType::Single, _) | (DelimiterType::Unquoted, 0) => proc.write(ENCODED[b'\'']),
                _ => proc.write(c),
            },
            // If double quoted, encode any double quote anywhere.
            // If unquoted, encode double quote if first character.
            CharType::DoubleQuote => match (optimal_delimiter, char_no) {
                (DelimiterType::Double, _) | (DelimiterType::Unquoted, 0) => proc.write(ENCODED[b'"']),
                _ => proc.write(c),
            },
            // If unquoted, encode right chevron if last character.
            CharType::RightChevron => if optimal_delimiter == DelimiterType::Unquoted && char_no == metrics.collected_count - 1 {
                proc.write(ENCODED[b'>']);
            } else {
                proc.write(b'>');
            },
        },
    );
    // Ensure closing delimiter in src has been matched and discarded, if any.
    if let Some(c) = src_delimiter {
        proc.match_char(c).expect().discard();
    }
    // Write closing delimiter, if any.
    if let Some(c) = optimal_delimiter_char {
        proc.write(c);
    }

    if optimal_delimiter != DelimiterType::Unquoted {
        Ok(AttrType::Unquoted)
    } else {
        Ok(AttrType::Quoted)
    }
}
