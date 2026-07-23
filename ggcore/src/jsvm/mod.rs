//! gg-js: the browser's hand-written JavaScript engine.
//!
//! Research notes and target numbers: docs/jsvm-research.md.
//! Planned pipeline: lexer -> parser -> bytecode compiler -> register VM
//! with NaN-boxed values, shapes + inline caches, interned atoms, and
//! DOM nodes as native VM values (no binding layer).
#![allow(dead_code)]

pub mod ast;
pub mod bytecode;
pub mod compiler;
pub mod lexer;
pub mod page;
pub mod parser;
pub mod value;
pub mod vm;

/// One-shot headless eval: returns the value of the last expression
/// statement plus everything console.log printed.
pub fn eval(src: &str) -> Result<(value::Value, Vec<String>), String> {
    page::PageVm::eval(src)
}
