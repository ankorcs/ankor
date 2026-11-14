//! The wire contract between a game-host agent and its controller: frame validation, types,
//! constants and helpers.

pub mod config;
pub mod constants;
pub mod helpers;
mod refine;
pub mod schemas;
#[doc(hidden)]
pub mod support;
pub mod types;
pub mod validate;
