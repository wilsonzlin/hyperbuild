use std::ops::{Index, IndexMut};

use fastrie::Fastrie;

use crate::err::{ErrorType, ProcessingResult};
use crate::pattern::SinglePattern;
use crate::proc::MatchAction::*;
use crate::proc::MatchCond::*;
use crate::proc::MatchMode::*;
use crate::proc::range::ProcessorRange;
use crate::spec::codepoint::is_whitespace;
use std::fmt::{Debug, Formatter};
use core::fmt;

pub mod checkpoint;
pub mod range;
pub mod uep;

pub enum MatchCond {
    Is,
    IsNot,
    While,
    WhileNot,
    // Through is like WhileNot followed by Is, but matches zero if Is is zero.
    // Useful for matching delimiter patterns. For example, matching Through "</script>" match everything up to and including the next "</script>", but would match zero if there is no "</script>".
    Through,
}

pub enum MatchMode {
    Char(u8),
    Pred(fn(u8) -> bool),
    Seq(&'static [u8]),
    Pat(&'static SinglePattern),
}

pub enum MatchAction {
    Keep,
    Discard,
    MatchOnly,
}

// Processing state of a file. Most fields are used internally and set during
// processing. Single use only; create one per processing.
pub struct Processor<'d> {
    code: &'d mut [u8],
    // Index of the next character to read.
    read_next: usize,
    // Index of the next unwritten space.
    write_next: usize,
}

impl<'d> Index<ProcessorRange> for Processor<'d> {
    type Output = [u8];

    fn index(&self, index: ProcessorRange) -> &Self::Output {
        &self.code[index.start..index.end]
    }
}

impl<'d> IndexMut<ProcessorRange> for Processor<'d> {
    fn index_mut(&mut self, index: ProcessorRange) -> &mut Self::Output {
        debug_assert!(index.end <= self.write_next);
        &mut self.code[index.start..index.end]
    }
}

impl<'d> Processor<'d> {
    // Constructor.
    pub fn new(code: &mut [u8]) -> Processor {
        Processor { write_next: 0, read_next: 0, code }
    }

    // INTERNAL APIs.
    // Bounds checking.
    fn _in_bounds(&self, offset: usize) -> bool {
        self.read_next + offset < self.code.len()
    }

    // Reading.
    /// Get the `offset` character from next.
    /// When `offset` is 0, the next character is returned.
    /// Panics. Does not check bounds for performance (e.g. already checked).
    fn _read_offset(&self, offset: usize) -> u8 {
        self.code[self.read_next + offset]
    }

    fn _maybe_read_offset(&self, offset: usize) -> Option<u8> {
        self.code.get(self.read_next + offset).map(|c| *c)
    }

    fn _maybe_read_slice_offset(&self, offset: usize, count: usize) -> Option<&[u8]> {
        if self._in_bounds(offset + count - 1) {
            Some(&self.code[self.read_next + offset..self.read_next + offset + count])
        } else {
            None
        }
    }

    // Shifting.
    /// Move next `amount` characters to output.
    /// Panics. Does not check bounds for performance (e.g. already checked).
    fn _shift(&mut self, amount: usize) -> () {
        // Optimisation: Don't shift if already there (but still update offsets).
        if self.read_next != self.write_next {
            self.code.copy_within(self.read_next..self.read_next + amount, self.write_next);
        };
        self.read_next += amount;
        self.write_next += amount;
    }

    fn _replace(&mut self, start: usize, end: usize, data: &[u8]) -> usize {
        debug_assert!(end <= self.read_next);
        let added = data.len() - (end - start);
        // Do not allow writing over source.
        debug_assert!(self.write_next + added <= self.read_next);
        self.code.copy_within(end..self.write_next, end + added);
        self.code[start..start + data.len()].copy_from_slice(data);
        // Don't need to update read_next as only data before it has changed.
        self.write_next += added;
        added
    }

    fn _insert(&mut self, at: usize, data: &[u8]) -> usize {
        self._replace(at, at, data)
    }

    // Matching.
    fn _one<C: FnOnce(u8) -> bool>(&mut self, cond: C) -> usize {
        self._maybe_read_offset(0).filter(|n| cond(*n)).is_some() as usize
    }
    fn _many<C: Fn(u8) -> bool>(&mut self, cond: C) -> usize {
        let mut count = 0usize;
        while self._maybe_read_offset(count).filter(|c| cond(*c)).is_some() {
            count += 1;
        };
        count
    }

    // Make expectation explicit, even for Maybe.
    pub fn m(&mut self, cond: MatchCond, mode: MatchMode, action: MatchAction) -> ProcessorRange {
        let count = match (cond, mode) {
            (Is, Char(c)) => self._one(|n| n == c),
            (IsNot, Char(c)) => self._one(|n| n != c),
            (While, Char(c)) => self._many(|n| n == c),
            (WhileNot, Char(c)) => self._many(|n| n != c),
            (Through, Char(c)) => self.code[self.read_next..].iter().position(|n| *n == c).map_or(0, |p| p + 1),

            (Is, Pred(p)) => self._one(|n| p(n)),
            (IsNot, Pred(p)) => self._one(|n| !p(n)),
            (While, Pred(p)) => self._many(|n| p(n)),
            (WhileNot, Pred(p)) => self._many(|n| !p(n)),
            (Through, Pred(p)) => self.code[self.read_next..].iter().position(|n| p(*n)).map_or(0, |p| p + 1),

            // Sequence matching is slow. If using in a loop, use Pat or Trie instead.
            (Is, Seq(seq)) => self._maybe_read_slice_offset(0, seq.len()).filter(|src| *src == seq).map_or(0, |_| seq.len()),
            (IsNot, Seq(seq)) => self._maybe_read_slice_offset(0, seq.len()).filter(|src| *src != seq).map_or(0, |_| seq.len()),
            (While, Seq(_)) => unimplemented!(),
            (WhileNot, Seq(_)) => unimplemented!(),
            (Through, Seq(_)) => unimplemented!(),

            (Is, Pat(_)) => unimplemented!(),
            (IsNot, Pat(_)) => unimplemented!(),
            (While, Pat(_)) => unimplemented!(),
            (WhileNot, Pat(pat)) => pat.match_against(&self.code[self.read_next..]).unwrap_or(self.code.len() - self.read_next),
            (Through, Pat(pat)) => pat.match_against(&self.code[self.read_next..]).map_or(0, |p| p + pat.len()),
        };
        // If keeping, match will be available in written range (which is better as source might eventually get overwritten).
        // If discarding, then only option is source range.
        let start = match action {
            Discard | MatchOnly => self.read_next,
            Keep => self.write_next,
        };
        match action {
            Discard => self.read_next += count,
            Keep => self._shift(count),
            MatchOnly => {}
        };

        ProcessorRange { start, end: start + count }
    }

    pub fn m_trie<V: 'static + Copy>(&mut self, trie: &Fastrie<V>, action: MatchAction) -> Option<V> {
        trie.longest_matching_prefix(&self.code[self.read_next..]).map(|m| {
            let count = m.end + 1;
            match action {
                Discard => self.read_next += count,
                Keep => self._shift(count),
                MatchOnly => {}
            };
            *m.value
        })
    }

    // PUBLIC APIs.
    // Bounds checking
    pub fn at_end(&self) -> bool {
        !self._in_bounds(0)
    }

    /// Get how many characters have been consumed from source.
    pub fn read_len(&self) -> usize {
        self.read_next
    }

    /// Get how many characters have been written to output.
    pub fn written_len(&self) -> usize {
        self.write_next
    }

    pub fn reserve_output(&mut self, amount: usize) -> () {
        self.write_next += amount;
    }

    // Looking ahead.
    /// Get the `offset` character from next.
    /// When `offset` is 0, the next character is returned.
    pub fn peek(&self, offset: usize) -> Option<u8> {
        self._maybe_read_offset(offset)
    }

    pub fn peek_many(&self, offset: usize, count: usize) -> Option<&[u8]> {
        self._maybe_read_slice_offset(offset, count)
    }

    // Consuming source characters.
    /// Skip and return the next character.
    /// Will result in an error if exceeds bounds.
    pub fn skip(&mut self) -> ProcessingResult<u8> {
        self._maybe_read_offset(0).map(|c| {
            self.read_next += 1;
            c
        }).ok_or(ErrorType::UnexpectedEnd)
    }

    pub fn skip_amount_expect(&mut self, amount: usize) -> () {
        debug_assert!(!self.at_end(), "skip known characters");
        self.read_next += amount;
    }

    pub fn skip_expect(&mut self) -> () {
        debug_assert!(!self.at_end(), "skip known character");
        self.read_next += 1;
    }

    // Writing characters directly.
    /// Write `c` to output. Will panic if exceeds bounds.
    pub fn write(&mut self, c: u8) -> () {
        self.code[self.write_next] = c;
        self.write_next += 1;
    }

    pub fn write_range(&mut self, s: ProcessorRange) -> ProcessorRange {
        let dest_start = self.write_next;
        let dest_end = dest_start + s.len();
        self.code.copy_within(s.start..s.end, dest_start);
        self.write_next = dest_end;
        ProcessorRange { start: dest_start, end: dest_end }
    }

    /// Write `s` to output. Will panic if exceeds bounds.
    pub fn write_slice(&mut self, s: &[u8]) -> () {
        self.code[self.write_next..self.write_next + s.len()].copy_from_slice(s);
        self.write_next += s.len();
    }

    pub fn write_utf8(&mut self, c: char) -> () {
        let mut encoded = [0u8; 4];
        c.encode_utf8(&mut encoded);
        self.write_slice(&encoded);
    }

    // Shifting characters.
    pub fn accept(&mut self) -> ProcessingResult<u8> {
        self._maybe_read_offset(0).map(|c| {
            self.code[self.write_next] = c;
            self.read_next += 1;
            self.write_next += 1;
            c
        }).ok_or(ErrorType::UnexpectedEnd)
    }

    pub fn accept_expect(&mut self) -> u8 {
        debug_assert!(!self.at_end());
        let c = self._read_offset(0);
        self.code[self.write_next] = c;
        self.read_next += 1;
        self.write_next += 1;
        c
    }

    pub fn accept_amount_expect(&mut self, count: usize) -> () {
        debug_assert!(self._in_bounds(count - 1));
        self._shift(count);
    }
}

impl Debug for Processor<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut lines = vec![(1, String::new())];
        let mut line_idx = 0;
        let mut indicator_line_idx_opt: Option<usize> = None;
        let mut line_cols = 0;
        let mut line_no = 1;
        for (i, &c) in self.code.iter().enumerate() {
            if i == self.read_next || i == self.write_next {
                let indicator_line_idx = if indicator_line_idx_opt.is_none() {
                    let indicator_line_idx = lines.len();
                    lines.push((-1, String::new()));
                    indicator_line_idx_opt = Some(indicator_line_idx);
                    indicator_line_idx
                } else if let Some(indicator_line_idx) = indicator_line_idx_opt {
                    indicator_line_idx
                } else {
                    unreachable!();
                };
                // At this point, `line_cols` is how many characters are on this line BEFORE this character.
                while line_cols > 0 && lines[indicator_line_idx].1.len() < line_cols {
                    lines[indicator_line_idx].1.push(' ');
                };
                lines[indicator_line_idx].1.push(if i == self.read_next && i == self.write_next {
                    'B'
                } else if i == self.read_next {
                    'R'
                } else {
                    'W'
                })
            };
            match c {
                b'\n' => {
                    lines[line_idx].1.push_str("⏎");
                    line_no += 1;
                    line_cols = 0;
                    line_idx = lines.len();
                    lines.push((line_no, String::new()));
                    indicator_line_idx_opt = None;
                }
                c => {
                    match c {
                        c if is_whitespace(c) => lines[line_idx].1.push('·'),
                        c if c >= b'!' && c <= b'~' => lines[line_idx].1.push(c as char),
                        _ => lines[line_idx].1.push('�'),
                    };
                    line_cols += 1;
                }
            };
        };
        let max_line_no_width = (line_no as f64).log10().ceil() as usize;
        for l in lines
            .iter()
            .map(|(line_no, line)| if *line_no == -1 {
                format!("{:>indent$}|{}\n", String::from_utf8(vec![b'>'; max_line_no_width]).unwrap(), line, indent = max_line_no_width)
            } else {
                format!("{:>indent$}|{}\n", line_no, line, indent = max_line_no_width)
            })
            // Don't use for_each as otherwise we can't return errors.
        {
            f.write_str(l.as_str())?;
        }
        Ok(())
    }
}