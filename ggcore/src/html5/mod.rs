//! A spec-compliant HTML5 parser: tokenizer, tree construction, and a
//! tree that can carry everything the spec produces.
//!
//! Measured against html5lib-tests (vendored under validation/html5lib)
//! rather than against intuition — the whole point of writing this
//! stage to the letter is that "correct" is a number here, not a
//! judgement call.

pub mod entities;
pub mod sink;
pub mod tokenizer;
pub mod tree;

pub use sink::{Ns, Quirks, Sink};
pub use tree::{parse, parse_fragment, Parsed};

#[cfg(test)]
mod html5lib_test;
