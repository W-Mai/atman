//! Portable flow program types.
#![no_std]

extern crate alloc;

pub mod ast;

pub use ast::File as Program;
