//! Shared parts of Biggest Fan, used by both binaries built from this
//! crate: `biggest-fan` (the GUI plus one-shot hardware commands) and
//! `biggest-fan-daemon` (the root-privileged control loop).

#![warn(missing_docs)]
#![warn(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::complexity)]
#![warn(clippy::nursery)]
#![warn(clippy::perf)]
#![warn(clippy::style)]

pub mod app;
pub mod case_view;
pub mod curve_graph;
pub mod dbus;
pub mod engine_setup;
pub mod fan_controls;
pub mod power;
pub mod stress;
pub mod trend_graph;
pub mod tune;
