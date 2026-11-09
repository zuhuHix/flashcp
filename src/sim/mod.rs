//! Trace-driven device simulator (see SIMULATION.md): [`trace`] records the device ops flashcp
//! issues, [`device`] lets a recording on an image file follow a real device's strategy.

pub mod calib;
pub mod cli;
pub mod device;
pub mod ftl;
pub mod model;
pub mod profiles;
pub mod replay;
pub mod trace;
pub mod workload;
