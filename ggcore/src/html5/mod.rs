//! A spec-compliant HTML5 parser: tokenizer, tree construction, and a
//! tree that can carry everything the spec produces.
//!
//! Measured against html5lib-tests (vendored under validation/html5lib)
//! rather than against intuition — the whole point of writing this
//! stage to the letter is that "correct" is a number here, not a
//! judgement call.
//!
//! The engine still runs the older parser in `crate::html`, so parts of
//! the surface here — the re-exports, the adapter into the engine's DOM
//! — have no caller yet. They are the boundary the switchover uses.
#![allow(dead_code)]

pub mod entities;
pub mod scripts;
pub mod sink;
pub mod tokenizer;
pub mod tree;

pub use tree::parse;
#[cfg(test)]
pub use tree::parse_fragment;

#[cfg(test)]
mod html5lib_test;
