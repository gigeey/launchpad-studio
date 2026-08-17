//! One-way legacy importer for `watch_contract`'s pre-typed-[`Predicate`]
//! `predicate.expr` string grammar: `not_empty(field)`, `contains(field,
//! 'literal')`, `equals(field, 'literal')`, `and(a, b)`, `or(a, b)`,
//! `not(a)` — string comparison in `contains`/`equals` case-insensitive.
//!
//! This module exists ONLY to convert a string in that grammar into the
//! typed [`Predicate`] `PredicateSpec` now stores everywhere. There are
//! exactly two legitimate callers of [`parse`]:
//!
//! - `PredicateSpec`'s own `Deserialize` impl, migrating an already-persisted
//!   contract whose JSON still carries the legacy `expr` string field
//!   (written before this crate had a typed `Predicate`).
//! - `ao_engine::agent_watch::author_contract`, converting a freshly
//!   authored proposal's `expr` — the authoring model still emits this
//!   grammar (see `PREDICATE_GRAMMAR` in that crate) — into the typed form
//!   at the moment a contract is first bound.
//!
//! Both callers parse once, at the boundary where a string this grammar
//! might still show up, and store the typed result from then on. Nothing in
//! the runtime tick loop (`ao_engine::agent_watch::run_contract_bound_tick`)
//! ever calls into this module — it evaluates `PredicateSpec::predicate`
//! directly via `crate::predicate::evaluate_predicate`. Do not add a third
//! caller: a new write path has no reason to ever produce an `expr` string
//! again.

// Only the test-only legacy evaluator below reads payload JSON; `parse` works
// purely on the expression string.
#[cfg(test)]
use serde_json::Value;

use crate::predicate::Predicate;
use crate::watch_contract::ContractError;

/// One node of a parsed legacy `expr`. Deliberately tiny and
/// non-Turing-complete, matching exactly what the retired string grammar
/// could express — see the module doc for the six supported forms.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LegacyPredicate {
    Contains { field: String, literal: String },
    Equals { field: String, literal: String },
    NotEmpty { field: String },
    And(Box<LegacyPredicate>, Box<LegacyPredicate>),
    Or(Box<LegacyPredicate>, Box<LegacyPredicate>),
    Not(Box<LegacyPredicate>),
}

struct LegacyPredicateParser {
    chars: Vec<char>,
    pos: usize,
}

impl LegacyPredicateParser {
    fn new(input: &str) -> Self {
        LegacyPredicateParser { chars: input.chars().collect(), pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), ContractError> {
        self.skip_ws();
        if self.peek() == Some(expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(ContractError::InvalidPredicate(format!(
                "expected '{expected}' at position {}",
                self.pos
            )))
        }
    }

    fn parse_ident(&mut self) -> Result<String, ContractError> {
        self.skip_ws();
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(ContractError::InvalidPredicate(format!("expected an identifier at position {start}")));
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    fn parse_string_literal(&mut self) -> Result<String, ContractError> {
        self.skip_ws();
        self.expect('\'')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('\\') => match self.bump() {
                    Some('\'') => out.push('\''),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => return Err(ContractError::InvalidPredicate("unterminated string literal".to_string())),
                },
                Some('\'') => break,
                Some(c) => out.push(c),
                None => return Err(ContractError::InvalidPredicate("unterminated string literal".to_string())),
            }
        }
        Ok(out)
    }

    fn parse_predicate(&mut self) -> Result<LegacyPredicate, ContractError> {
        let name = self.parse_ident()?;
        self.expect('(')?;
        let predicate = match name.as_str() {
            "contains" => {
                let field = self.parse_ident()?;
                self.expect(',')?;
                let literal = self.parse_string_literal()?;
                LegacyPredicate::Contains { field, literal }
            }
            "equals" => {
                let field = self.parse_ident()?;
                self.expect(',')?;
                let literal = self.parse_string_literal()?;
                LegacyPredicate::Equals { field, literal }
            }
            "not_empty" => LegacyPredicate::NotEmpty { field: self.parse_ident()? },
            "and" => {
                let left = self.parse_predicate()?;
                self.expect(',')?;
                let right = self.parse_predicate()?;
                LegacyPredicate::And(Box::new(left), Box::new(right))
            }
            "or" => {
                let left = self.parse_predicate()?;
                self.expect(',')?;
                let right = self.parse_predicate()?;
                LegacyPredicate::Or(Box::new(left), Box::new(right))
            }
            "not" => LegacyPredicate::Not(Box::new(self.parse_predicate()?)),
            other => return Err(ContractError::InvalidPredicate(format!("unknown function \"{other}\""))),
        };
        self.expect(')')?;
        Ok(predicate)
    }
}

/// Field reader for the retired string evaluator. Test-only, for the same
/// reason as [`evaluate_legacy_expr`], which is its only caller.
#[cfg(test)]
fn field_as_string(payload: &Value, field: &str) -> Option<String> {
    match payload.get(field) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

/// Evaluation half of the retired string evaluator. Test-only, for the same
/// reason as [`evaluate_legacy_expr`], which is its only caller.
#[cfg(test)]
fn eval_legacy_predicate(predicate: &LegacyPredicate, payload: &Value) -> bool {
    match predicate {
        LegacyPredicate::Contains { field, literal } => field_as_string(payload, field)
            .map(|value| value.to_lowercase().contains(&literal.to_lowercase()))
            .unwrap_or(false),
        LegacyPredicate::Equals { field, literal } => field_as_string(payload, field)
            .map(|value| value.to_lowercase() == literal.to_lowercase())
            .unwrap_or(false),
        LegacyPredicate::NotEmpty { field } => match payload.get(field) {
            Some(Value::Null) | None => false,
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            Some(_) => true,
        },
        LegacyPredicate::And(a, b) => eval_legacy_predicate(a, payload) && eval_legacy_predicate(b, payload),
        LegacyPredicate::Or(a, b) => eval_legacy_predicate(a, payload) || eval_legacy_predicate(b, payload),
        LegacyPredicate::Not(a) => !eval_legacy_predicate(a, payload),
    }
}

/// Converts a parsed legacy AST into the typed [`Predicate`] this crate now
/// stores. `Contains`/`Equals` map onto [`Predicate::ContainsCi`]/
/// [`Predicate::EqualsCi`] rather than [`Predicate::Contains`]/
/// [`Predicate::Equals`] — the legacy grammar's comparisons are
/// case-insensitive and string-coercing (see [`eval_legacy_predicate`]),
/// which is exactly what the `Ci` variants preserve; `Predicate::Contains`/
/// `Equals` are case-sensitive typed-value comparisons and would silently
/// change firing behavior for any legacy expression that relied on case
/// folding. `And`/`Or` (binary in the legacy grammar) map onto the typed
/// enum's n-ary `Vec` form with exactly two elements — evaluating an n-ary
/// AND/OR over two elements is identical to evaluating the binary form.
fn to_typed(predicate: &LegacyPredicate) -> Predicate {
    match predicate {
        LegacyPredicate::Contains { field, literal } => {
            Predicate::ContainsCi { path: field.clone(), literal: literal.clone() }
        }
        LegacyPredicate::Equals { field, literal } => {
            Predicate::EqualsCi { path: field.clone(), literal: literal.clone() }
        }
        LegacyPredicate::NotEmpty { field } => Predicate::NotEmpty { path: field.clone() },
        LegacyPredicate::And(a, b) => Predicate::And(vec![to_typed(a), to_typed(b)]),
        LegacyPredicate::Or(a, b) => Predicate::Or(vec![to_typed(a), to_typed(b)]),
        LegacyPredicate::Not(a) => Predicate::Not(Box::new(to_typed(a))),
    }
}

/// Parses `expr` (the legacy string grammar) into the typed [`Predicate`]
/// this crate now stores — the migration entry point; see the module doc
/// for its two legitimate callers. Same failure mode as the retired
/// string-based evaluator: an unparseable expression or unknown function
/// name is `Err(ContractError::InvalidPredicate)`, never silently accepted
/// (see [`evaluate_legacy_expr`]'s differential tests, which pin this).
pub fn parse(expr: &str) -> Result<Predicate, ContractError> {
    let mut parser = LegacyPredicateParser::new(expr);
    let predicate = parser.parse_predicate()?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(ContractError::InvalidPredicate(format!(
            "unexpected trailing input at position {}",
            parser.pos
        )));
    }
    Ok(to_typed(&predicate))
}

/// Retained ONLY to prove — in the differential test battery — that
/// [`parse`]'s typed output evaluates identically to what the retired
/// string-based runtime evaluator produced for the same `expr`/`payload`
/// pair. Reproduces that evaluator's exact behavior, including its exact
/// parse-failure error text. Never call this for anything else: it is not
/// wired into any runtime path, and `pub(crate)` keeps it that way outside
/// this crate's own test suite. Compiled only under `cfg(test)`, so the
/// "not wired into any runtime path" claim above is enforced rather than
/// merely asserted — it does not exist in a release build.
#[doc(hidden)]
#[cfg(test)]
pub(crate) fn evaluate_legacy_expr(expr: &str, payload: &Value) -> Result<bool, ContractError> {
    let mut parser = LegacyPredicateParser::new(expr);
    let predicate = parser.parse_predicate()?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(ContractError::InvalidPredicate(format!(
            "unexpected trailing input at position {}",
            parser.pos
        )));
    }
    Ok(eval_legacy_predicate(&predicate, payload))
}

#[cfg(test)]
#[path = "legacy_expr_tests.rs"]
mod tests;
