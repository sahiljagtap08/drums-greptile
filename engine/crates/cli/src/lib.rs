//! Library crate for `drums-watch`: the Stage-1 pipeline orchestrator
//! (`engine`) and the §7-style terminal narration renderer (`render`).
//! Split out so `pub` items are real crate API (exempt from dead-code
//! analysis) rather than dead weight hanging off an empty `main`. The
//! `drums` binary (Task 8) wires these together.

pub mod account;
pub mod api;
pub mod app_root;
pub mod behavior_source;
pub mod bet_cmd;
pub mod change_cmd;
pub mod config;
pub mod daemon;
pub mod dashboard;
pub mod digest;
pub mod dispatch;
pub mod doctor;
pub mod draft;
pub mod engine;
pub mod hypothesize;
pub mod license;
pub mod login;
pub mod notify;
pub mod open;
pub(crate) mod proc;
pub mod record_cmd;
pub mod render;
pub mod repair_cmd;
pub mod restore;
pub mod setup;
pub mod ship;
pub mod signal;
pub mod sync;
pub mod telemetry;
pub mod tracker_poll;
pub mod ui;
pub mod why;
pub mod wire;
