#![allow(clippy::derive_partial_eq_without_eq)]

#[allow(unused_imports)]
#[macro_use]
extern crate log;
#[allow(unused_imports)]
#[macro_use]
extern crate prost_derive;

pub mod kvraft;
mod proto;
pub mod raft;

/// A place holder for suppressing unused_variables warning.
#[allow(dead_code)]
fn your_code_here<T>(_: T) -> ! {
    unimplemented!()
}
