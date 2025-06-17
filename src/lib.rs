#![feature(likely_unlikely)]
#![feature(step_trait)]
#![feature(let_chains)]
#![warn(missing_docs)]

//! A set of tools and implementations related to LDOS table based communication.

// Allow importing alloc, to allow code closer to Asterinas to reduce porting overhead later.
extern crate alloc;

pub mod table_ipc;
pub mod stubs;
