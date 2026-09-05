//! The `Sampler` trait: one implementation per signal source.
//!
//! Each sampler owns its own persistent state (previous snapshots, IOKit /
//! IOReport / Mach handles in later milestones) and, on each `tick`, returns
//! the deltas observed since the previous tick. The first tick of a stateful
//! sampler may return an empty / baseline result while it primes its state.

use std::time::Duration;

/// A source of power/heat telemetry. Stateful: keeps its own previous snapshot
/// and diffs against it on each tick.
pub trait Sampler {
    /// Human-readable name for diagnostics. Used by the milestone-2 multi-
    /// sampler driver; kept on the trait so every source self-identifies.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
}

/// Sampler that produces per-process rows.
pub trait ProcSampler: Sampler {
    /// Take a fresh snapshot, diff against the stored previous one, and return
    /// per-process deltas. `dt` is the elapsed wall time since the previous
    /// snapshot, used to convert cumulative counters into rates.
    fn tick(&mut self, dt: Duration) -> Vec<crate::types::ProcRow>;
}

/// Sampler that produces a single whole-system battery frame.
pub trait BatterySampler: Sampler {
    /// Read the current battery / power-source state. Returns `None` if the
    /// AppleSmartBattery service is unavailable (e.g. a desktop Mac).
    fn read(&mut self) -> Option<crate::types::BatteryFrame>;
}

/// Sampler that produces a system-wide SoC power frame (IOReport).
pub trait PowerSampler: Sampler {
    /// Return the SoC power integrated over the elapsed interval. `dt` is the
    /// time between the previous and current raw samples, used to turn the
    /// energy delta into watts. Returns `None` if IOReport is unavailable.
    fn tick(&mut self, dt: Duration) -> Option<crate::types::SocPower>;
}

/// Sampler that produces a thermal frame (SMC die temps + thermal pressure).
pub trait ThermalSampler: Sampler {
    /// Read current die temperatures, PSTR system power and thermal pressure.
    /// These are instantaneous values, so no interval is needed.
    fn read(&mut self) -> crate::types::ThermalFrame;
}

// The GPU sampler (samplers::agx::AgxSampler) uses a poll-based interface
// (begin/poll/finish) so its submitter histogram can run during the existing
// interval rather than adding a sleep, so it does not fit the simple
// one-shot trait shape above and is driven concretely from the main loop.
