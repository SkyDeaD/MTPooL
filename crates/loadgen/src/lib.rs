//! Load generator for the registrar: simulated nodes that behave as the agent does (same
//! requests, same intervals) and public viewers that poll the pool pages.

pub mod args;
pub mod model;
pub mod procfs;
pub mod run;
pub mod stats;
