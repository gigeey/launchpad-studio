use std::fmt;

use serde::Serialize as _;
use serde_json::Value;

/// Wraps a parsed .ipynb file as a loose `serde_json::Value`.
///
/// All top-level keys (nbformat_minor, metadata, kernel info, etc.) are
/// preserved verbatim — no field destructuring that could silently drop them.
#[derive(Debug)]
pub struct Notebook {
    value: Value,
}

/// Errors produced by notebook parsing and cell-id resolution.
#[derive(Debug)]
pub enum IpynbError {
    ParseJson(serde_json::Error),
    Utf16Bom,
    CellsNotArray,
    CellIdNotFound { input: String },
    IndexOutOfBounds { index: usize, len: usize },
}

impl fmt::Display for IpynbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpynbError::ParseJson(e) => write!(f, "Failed to parse notebook JSON: {e}"),
            IpynbError::Utf16Bom => {
                write!(f, "Notebook file is not UTF-8 (UTF-16 BOM detected)")
            }
            IpynbError::CellsNotArray => {
                write!(f, "Notebook 'cells' field is missing or not an array")
            }
            IpynbError::CellIdNotFound { input } => {
                write!(f, "Cell id '{input}' not found in notebook")
            }
            IpynbError::IndexOutOfBounds { index, len } => {
                write!(
                    f,
                    "Cell index {index} is out of bounds (notebook has {len} cells)"
                )
            }
        }
    }
}

impl Notebook {
    /// Parse a `.ipynb` file from its raw bytes.
    ///
    /// Returns `IpynbError::Utf16Bom` if a UTF-16 BOM is detected at offset 0.
    /// Returns `IpynbError::ParseJson` for any JSON decoding failure.
    pub fn parse(bytes: &[u8]) -> Result<Notebook, IpynbError> {
        if bytes.len() >= 2
            && ((bytes[0] == 0xFE && bytes[1] == 0xFF) || (bytes[0] == 0xFF && bytes[1] == 0xFE))
        {
            return Err(IpynbError::Utf16Bom);
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| serde_json::from_str::<Value>("").unwrap_err())
            .map_err(IpynbError::ParseJson)?;
        let value: Value = serde_json::from_str(text).map_err(IpynbError::ParseJson)?;
        Ok(Notebook { value })
    }

    /// Emit pretty-printed JSON with a 1-space indent and a trailing newline.
    ///
    /// Matches Jupyter's default serialisation format.
    pub fn serialise(&self) -> String {
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut buf = Vec::new();
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        self.value
            .serialize(&mut ser)
            .expect("serde_json::Value is always serialisable");
        let mut out = String::from_utf8(buf).expect("serde_json output is always UTF-8");
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Return the cells array as a slice.
    ///
    /// Returns an empty slice if `cells` is missing or not an array.
    pub fn cells(&self) -> &[Value] {
        match self.value.get("cells").and_then(Value::as_array) {
            Some(arr) => arr.as_slice(),
            None => &[],
        }
    }

    /// Return a mutable reference to the cells array.
    ///
    /// Errors with `IpynbError::CellsNotArray` if the top-level `cells` key is
    /// missing or not a JSON array.
    pub fn cells_mut(&mut self) -> Result<&mut Vec<Value>, IpynbError> {
        match self.value.get_mut("cells").and_then(Value::as_array_mut) {
            Some(arr) => Ok(arr),
            None => Err(IpynbError::CellsNotArray),
        }
    }

    /// Resolve a `cell_id` string to a 0-based cell index.
    ///
    /// 1. If `input` parses as `usize`, range-check against cells length.
    /// 2. Otherwise, scan for the first cell whose `"id"` field matches.
    pub fn resolve_cell_id(&self, input: &str) -> Result<usize, IpynbError> {
        let cells = self.cells();
        match input.parse::<usize>() {
            Ok(idx) => {
                if idx < cells.len() {
                    Ok(idx)
                } else {
                    Err(IpynbError::IndexOutOfBounds {
                        index: idx,
                        len: cells.len(),
                    })
                }
            }
            Err(_) => {
                for (i, cell) in cells.iter().enumerate() {
                    if cell.get("id").and_then(Value::as_str) == Some(input) {
                        return Ok(i);
                    }
                }
                Err(IpynbError::CellIdNotFound {
                    input: input.to_string(),
                })
            }
        }
    }
}
