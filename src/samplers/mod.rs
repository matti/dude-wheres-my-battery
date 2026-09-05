//! Concrete `Sampler` implementations, one module per signal source.
//!
//! Milestone 1: per-process rusage + battery. Milestone 2: cross-uid process
//! identity. Milestone 3: `ioreport` for system CPU/GPU/ANE/DRAM power.
//! Milestone 4: `thermal` for SMC die temps + thermal pressure.
//! Milestone 6: `agx` for sudoless GPU attribution (top submitter + busy%).
//! Milestone 7: `io` for system-wide disk + network throughput.

pub mod agx;
pub mod battery;
pub mod cf;
pub mod io;
pub mod ioreport;
pub mod proc_rusage;
pub mod procname;
pub mod thermal;
