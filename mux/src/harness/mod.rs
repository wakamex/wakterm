//! Each harness's own code: how Wakterm finds its session, reads it, and
//! reads and answers its screen. The facts every harness has, such as its
//! programs and launch command, are in `crate::agent::HarnessSpec`.

pub mod devin;
