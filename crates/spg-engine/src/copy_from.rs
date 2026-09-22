//! 9.0.3 — the rows of `COPY … FROM`, read the one way every host reads
//! them.
//!
//! Five places ran a COPY FROM loop — the wire's STDIN stream, the
//! server's and the embedded host's file endpoint,
//! `Engine::copy_from_buffer`, and the embedded dump import — and each
//! decoded records its own way: every one skipped an empty line, which
//! PostgreSQL reads as a row; the dump import ignored every option; none
//! honoured `ESCAPE` on the way in. Each host still runs the INSERTs
//! itself, because it owns the WAL and the transaction. What a record
//! MEANS is decided here.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub use spg_sql::ast::CopyOptions;
use spg_sql::ast::{CopyFormat, CopyLogVerbosity, CopyOnError};

use crate::EngineError;

/// One record of input, decoded.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyRecord {
    /// A data row, and the input line it started on (1-based, counting
    /// the header line, as PostgreSQL's `line N` does).
    Row {
        line: u64,
        values: Vec<Option<String>>,
    },
    /// The header line, or a row `SKIP` drops.
    Consumed,
    /// `\.` — the end of the data.
    End,
}

/// The relation a COPY FROM fills, and the columns each row carries.
#[derive(Debug, Clone)]
pub struct CopyTarget {
    /// The relation's key, as the statement named it.
    pub table: String,
    /// Whether the statement wrote a schema in front of it.
    pub qualified: bool,
    /// The column list the statement wrote, if any.
    pub columns: Option<Vec<String>>,
    /// The columns each row fills, in order: the written list, or every
    /// column of the table.
    pub names: Vec<String>,
    /// Every column of the table, in order.
    pub table_columns: Vec<String>,
}

/// What becomes of one decoded row.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyRowAction {
    /// Run this INSERT.
    Insert(String),
    /// Leave the row out, sending this notice if there is one.
    Skip(Option<String>),
}

impl CopyTarget {
    /// PostgreSQL checks a row carries one value per target column before
    /// any value is converted.
    ///
    /// # Errors
    /// `missing data for column "x"` / `extra data after last expected
    /// column` (22P04).
    pub fn check_arity(&self, values: &[Option<String>]) -> Result<(), EngineError> {
        if values.len() > self.names.len() {
            return Err(EngineError::Unsupported(String::from(
                "extra data after last expected column",
            )));
        }
        if values.len() < self.names.len() {
            return Err(EngineError::Unsupported(format!(
                "missing data for column \"{}\"",
                self.names[values.len()]
            )));
        }
        Ok(())
    }

    /// The INSERT that stores `values`.
    #[must_use]
    pub fn insert_sql(&self, values: &[Option<String>]) -> String {
        crate::copy::build_copy_insert(&self.table, self.qualified, self.columns.as_deref(), values)
    }
}

/// Reads COPY FROM records against one target column list.
#[derive(Debug)]
pub struct CopyFromReader<'a> {
    options: &'a CopyOptions,
    target: &'a [String],
    csv: bool,
    delimiter: char,
    quote: char,
    escape: char,
    null_str: String,
    header_pending: bool,
    skip_left: u64,
    next_line: u64,
}

impl<'a> CopyFromReader<'a> {
    /// A reader for `target`, the columns each row fills in order.
    ///
    /// # Errors
    /// PostgreSQL's refusals for an option COPY FROM cannot take here.
    pub fn new(options: &'a CopyOptions, target: &'a CopyTarget) -> Result<Self, EngineError> {
        crate::copy::validate_copy_option_direction(options, false)?;
        let csv = match options.format {
            CopyFormat::Csv => true,
            CopyFormat::Text => false,
            // SPG's own format: the wire decodes it line by line into an
            // INSERT of its own, and nothing else reads it.
            _ => {
                return Err(EngineError::Unsupported(
                    "COPY FORMAT json is supported only by COPY FROM STDIN".into(),
                ));
            }
        };
        if !csv {
            if options.quote.is_some() {
                return Err(EngineError::Unsupported(
                    "COPY QUOTE requires CSV mode".into(),
                ));
            }
            if options.escape.is_some() {
                return Err(EngineError::Unsupported(
                    "COPY ESCAPE requires CSV mode".into(),
                ));
            }
        }
        for (list, name) in [
            (&options.force_not_null, "FORCE_NOT_NULL"),
            (&options.force_null, "FORCE_NULL"),
        ] {
            for c in list.iter().flatten() {
                crate::copy::check_listed_column(name, c, target)?;
            }
        }
        let quote = options.quote.unwrap_or('"');
        Ok(Self {
            options,
            target: &target.names,
            csv,
            delimiter: options.delimiter.unwrap_or(if csv { ',' } else { '\t' }),
            quote,
            escape: options.escape.unwrap_or(quote),
            null_str: options
                .null_str
                .clone()
                .unwrap_or_else(|| String::from(if csv { "" } else { "\\N" })),
            header_pending: options.header,
            skip_left: options.skip,
            next_line: 1,
        })
    }

    /// Byte length of the first complete record in `buf`, its newline
    /// included, or `None` while the buffer does not hold one yet.
    #[must_use]
    pub fn record_end(&self, buf: &[u8]) -> Option<usize> {
        self.splitter().end(buf)
    }

    /// Where this reader's records end, as a value a host can hold while
    /// the reader itself is borrowed.
    #[must_use]
    pub fn splitter(&self) -> CopyRecordSplit {
        CopyRecordSplit {
            csv: self.csv,
            delimiter: self.delimiter as u8,
            quote: self.quote as u8,
            escape: self.escape as u8,
        }
    }

    /// Decode one record, given without its terminating newline.
    ///
    /// # Errors
    /// `HEADER match` refusals, in PostgreSQL's words.
    pub fn read(&mut self, record: &str) -> Result<CopyRecord, EngineError> {
        let record = record.strip_suffix('\r').unwrap_or(record);
        let line = self.next_line;
        self.next_line += 1 + record.matches('\n').count() as u64;
        if record == "\\." {
            return Ok(CopyRecord::End);
        }
        let values = self.decode(record);
        if self.header_pending {
            self.header_pending = false;
            if self.options.header_match {
                self.match_header(&values)?;
            }
            return Ok(CopyRecord::Consumed);
        }
        if self.skip_left > 0 {
            self.skip_left -= 1;
            return Ok(CopyRecord::Consumed);
        }
        Ok(CopyRecord::Row { line, values })
    }

    fn decode(&self, record: &str) -> Vec<Option<String>> {
        if !self.csv {
            return crate::copy::decode_copy_text_row_opts(record, self.delimiter, &self.null_str);
        }
        let mut values = crate::copy::decode_copy_csv_record_escaped(
            record,
            self.delimiter,
            self.quote,
            self.escape,
            &self.null_str,
        );
        // FORCE_NOT_NULL reads a field that decoded as NULL as the empty
        // string; FORCE_NULL reads one equal to the null token — which only
        // a QUOTED field can still be here — as NULL.
        let listed = |list: &Option<Vec<String>>, idx: usize| match list {
            None => false,
            Some(cols) if cols.is_empty() => true,
            Some(cols) => self
                .target
                .get(idx)
                .is_some_and(|c| cols.iter().any(|w| w.eq_ignore_ascii_case(c))),
        };
        for (idx, cell) in values.iter_mut().enumerate() {
            if listed(&self.options.force_not_null, idx) && cell.is_none() {
                *cell = Some(String::new());
            }
            if listed(&self.options.force_null, idx)
                && cell.as_deref() == Some(self.null_str.as_str())
            {
                *cell = None;
            }
        }
        values
    }

    fn match_header(&self, names: &[Option<String>]) -> Result<(), EngineError> {
        if names.len() != self.target.len() {
            return Err(EngineError::Unsupported(format!(
                "wrong number of fields in header line: got {}, expected {}",
                names.len(),
                self.target.len()
            )));
        }
        for (i, (got, want)) in names.iter().zip(self.target).enumerate() {
            match got {
                Some(g) if g == want => {}
                Some(g) => {
                    return Err(EngineError::Unsupported(format!(
                        "column name mismatch in header line field {}: got \"{g}\", expected \"{want}\"",
                        i + 1
                    )));
                }
                None => {
                    return Err(EngineError::Unsupported(format!(
                        "column name mismatch in header line field {}: got null value (\"{}\"), expected \"{want}\"",
                        i + 1,
                        self.null_str
                    )));
                }
            }
        }
        Ok(())
    }
}

impl CopyFromReader<'_> {
    /// Every data row of a whole buffer — a file the host read, or the
    /// lines a dump carries after its COPY statement. A last line without
    /// its newline is a row too.
    ///
    /// # Errors
    /// The first `HEADER match` refusal, or data that is not UTF-8.
    pub fn read_all(
        &mut self,
        data: &[u8],
    ) -> Result<Vec<(u64, Vec<Option<String>>)>, EngineError> {
        let mut rows = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let (record, next) = match self.record_end(rest) {
                Some(end) => (&rest[..end - 1], &rest[end..]),
                None => (rest, &rest[rest.len()..]),
            };
            rest = next;
            let text = core::str::from_utf8(record).map_err(|_| {
                EngineError::Unsupported("invalid byte sequence for encoding \"UTF8\"".into())
            })?;
            match self.read(text)? {
                CopyRecord::Row { line, values } => rows.push((line, values)),
                CopyRecord::Consumed => {}
                CopyRecord::End => break,
            }
        }
        Ok(rows)
    }
}

/// Where COPY FROM records end: at a newline, except that a CSV record
/// runs past one inside a quoted field.
#[derive(Debug, Clone, Copy)]
pub struct CopyRecordSplit {
    csv: bool,
    delimiter: u8,
    quote: u8,
    escape: u8,
}

impl CopyRecordSplit {
    /// Byte length of the first complete record in `buf`, its newline
    /// included, or `None` while the buffer does not hold one yet.
    #[must_use]
    pub fn end(&self, buf: &[u8]) -> Option<usize> {
        if self.csv {
            crate::copy::csv_record_end_escaped(buf, self.delimiter, self.quote, self.escape)
        } else {
            buf.iter().position(|&b| b == b'\n').map(|i| i + 1)
        }
    }
}

/// What a row that failed does to the COPY under `ON_ERROR`.
#[derive(Debug)]
pub struct CopyRowErrors {
    on_error: CopyOnError,
    verbosity: CopyLogVerbosity,
    reject_limit: Option<u64>,
    skipped: u64,
}

impl CopyRowErrors {
    #[must_use]
    pub fn new(options: &CopyOptions) -> Self {
        Self {
            on_error: options.on_error.unwrap_or_default(),
            verbosity: options.log_verbosity,
            reject_limit: options.reject_limit,
            skipped: 0,
        }
    }

    /// True when a cell a column cannot read skips its row rather than
    /// ending the COPY — so the host must look before it inserts.
    #[must_use]
    pub fn checks_input(&self) -> bool {
        self.on_error == CopyOnError::Ignore
    }

    /// True when a row that fails for any reason is dropped (SPG's
    /// `ON_ERROR set_null`).
    #[must_use]
    pub fn drops_any_failure(&self) -> bool {
        self.on_error == CopyOnError::SetNull
    }

    /// `column` could not read `value` on input line `line`: the row is
    /// skipped. Returns the notice to send for it, if any.
    ///
    /// # Errors
    /// More rows skipped than `REJECT_LIMIT` allows.
    pub fn skip(
        &mut self,
        line: u64,
        column: &str,
        value: &str,
    ) -> Result<Option<String>, EngineError> {
        self.skipped += 1;
        if let Some(limit) = self.reject_limit
            && self.skipped > limit
        {
            return Err(EngineError::Unsupported(format!(
                "skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"
            )));
        }
        Ok((self.verbosity == CopyLogVerbosity::Verbose).then(|| {
            format!(
                "skipping row due to data type incompatibility at line {line} for column \"{column}\": \"{value}\""
            )
        }))
    }

    /// The notice that closes a COPY which skipped rows, if any.
    #[must_use]
    pub fn closing_notice(&self) -> Option<String> {
        if self.skipped == 0 || self.verbosity == CopyLogVerbosity::Silent {
            return None;
        }
        Some(if self.skipped == 1 {
            String::from("1 row was skipped due to data type incompatibility")
        } else {
            format!(
                "{} rows were skipped due to data type incompatibility",
                self.skipped
            )
        })
    }
}
