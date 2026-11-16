//! libva bindings generated at build time (see `build.rs`).
#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unsafe_op_in_unsafe_fn,
    clippy::all,
    clippy::pedantic
)]

include!(concat!(env!("OUT_DIR"), "/va.rs"));
