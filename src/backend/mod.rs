//! Execution backend: SQL AST, DuckDB lowering, the plan executor and relation comparison.

pub mod compare;
pub mod duckdb;
pub mod run;
pub mod sql;
