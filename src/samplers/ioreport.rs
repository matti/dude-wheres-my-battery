//! System-wide SoC power via the private `IOReport` framework (sudoless).
//!
//! IOReport is the same no-root backbone macmon uses: it exposes the SoC's
//! internal performance counters grouped by IP block. We subscribe to the
//! "Energy Model" group (CPU/GPU/ANE/DRAM energy, integrated -> watts) and to
//! the "CPU Stats" / "GPU Stats" groups (DVFS state-residency channels ->
//! active frequency + busy fraction).
//!
//! FFI signatures are transcribed from macmon's `sources.rs` (MIT,
//! github.com/vladkens/macmon), the canonical verified reference - there is no
//! crate wrapping IOReport. Channels are matched at RUNTIME by group / channel
//! NAME, never by hardcoded index, because the M5 Pro channel layout differs
//! from the M1-M4 macmon targets; a missing channel degrades to `None`.
//!
//! Energy -> watts: each channel reports an integer in the units named by its
//! UnitLabel (nJ / uJ / mJ). Convert to joules, divide by the interval seconds.
//!
//! M5 Pro Energy Model channel layout (enumerated live, NOT hardcoded):
//!   * CPU: many per-core `PCPUDTL*` (P-cluster) + cluster channels (mJ).
//!   * GPU: a coarse `GPU` (mJ) AND a precise `GPU Energy` (nJ). We count the
//!     `GPU Energy` channel and ignore the coarse `GPU` to avoid double-count.
//!   * `ANE` (mJ), `DRAM`/`DCS`/`AMCC` (memory subsystem), `DISP` (display
//!     rail), plus `ISP`/`AVE`/`MSR`/`FAB`/`PCIe*` we lump into "other".
//!
//! DVFS state labels are voltage/pstate codes (`V0P19`...), not MHz, and the
//! unit is `24Mticks`; idle states are `DOWN`/`IDLE`/`OFF`. We therefore report
//! active residency (the busy fraction) and the top active P-state index, not a
//! parsed frequency, since labels carry no MHz on this chip.
//!
//! All CoreFoundation / IOReport refs live entirely inside this sampler (which
//! is owned by the single dedicated sampling thread); only the POD `SocPower`
//! leaves. RAII `Drop` releases the subscription / channel dictionaries.

use std::ptr::null;
use std::time::Duration;

use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{
    CFDictionaryCreateMutableCopy, CFDictionaryGetCount, CFDictionaryGetValue, CFDictionaryRef,
    CFMutableDictionaryRef,
};
use core_foundation_sys::string::CFStringRef;

use crate::sampler::{PowerSampler, Sampler};
use crate::samplers::cf::{cfstr, from_cfstr};
use crate::types::SocPower;

// ---------------------------------------------------------------------------
// FFI (signatures per macmon sources.rs, verified reference).
// ---------------------------------------------------------------------------

type CVoidRef = *const std::ffi::c_void;

#[repr(C)]
struct IOReportSubscription {
    _data: [u8; 0],
}
type IOReportSubscriptionRef = *const IOReportSubscription;

#[link(name = "IOReport", kind = "dylib")]
unsafe extern "C" {
    fn IOReportCopyChannelsInGroup(
        group: CFStringRef,
        subgroup: CFStringRef,
        c: u64,
        d: u64,
        e: u64,
    ) -> CFDictionaryRef;
    fn IOReportMergeChannels(a: CFDictionaryRef, b: CFDictionaryRef, nil: CFTypeRef);
    fn IOReportCreateSubscription(
        a: CVoidRef,
        channels: CFMutableDictionaryRef,
        out: *mut CFMutableDictionaryRef,
        d: u64,
        nil: CFTypeRef,
    ) -> IOReportSubscriptionRef;
    fn IOReportCreateSamples(
        subs: IOReportSubscriptionRef,
        channels: CFMutableDictionaryRef,
        nil: CFTypeRef,
    ) -> CFDictionaryRef;
    fn IOReportCreateSamplesDelta(
        prev: CFDictionaryRef,
        next: CFDictionaryRef,
        nil: CFTypeRef,
    ) -> CFDictionaryRef;
    fn IOReportChannelGetGroup(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetSubGroup(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetChannelName(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetUnitLabel(item: CFDictionaryRef) -> CFStringRef;
    fn IOReportSimpleGetIntegerValue(item: CFDictionaryRef, b: i32) -> i64;
    fn IOReportStateGetCount(item: CFDictionaryRef) -> i32;
    fn IOReportStateGetNameForIndex(item: CFDictionaryRef, index: i32) -> CFStringRef;
    fn IOReportStateGetResidency(item: CFDictionaryRef, index: i32) -> i64;
}

/// Borrowed dictionary lookup by string key.
fn dict_get(dict: CFDictionaryRef, key: &str) -> Option<CFTypeRef> {
    let k = cfstr(key);
    let v = unsafe { CFDictionaryGetValue(dict, k as *const _) };
    unsafe { CFRelease(k as CFTypeRef) };
    if v.is_null() {
        None
    } else {
        Some(v as CFTypeRef)
    }
}

// ---------------------------------------------------------------------------
// Channel subscription, owned with RAII.
// ---------------------------------------------------------------------------

/// Build the merged channel dictionary for the requested (group, subgroup)
/// pairs. Returns `None` if no channels could be retrieved.
fn build_channels(groups: &[(&str, Option<&str>)]) -> Option<CFMutableDictionaryRef> {
    let mut raw: Vec<CFDictionaryRef> = Vec::new();
    for (group, subgroup) in groups {
        let g = cfstr(group);
        let s = subgroup.map_or(null(), cfstr);
        let chan = unsafe { IOReportCopyChannelsInGroup(g, s, 0, 0, 0) };
        unsafe { CFRelease(g as CFTypeRef) };
        if subgroup.is_some() {
            unsafe { CFRelease(s as CFTypeRef) };
        }
        if !chan.is_null() {
            raw.push(chan);
        }
    }
    if raw.is_empty() {
        return None;
    }

    // Merge subsequent channel dicts into the first, then make a mutable copy.
    let base = raw[0];
    for &c in &raw[1..] {
        unsafe { IOReportMergeChannels(base, c, null()) };
    }
    let size = unsafe { CFDictionaryGetCount(base) };
    let merged = unsafe { CFDictionaryCreateMutableCopy(kCFAllocatorDefault, size, base) };
    for &c in &raw {
        unsafe { CFRelease(c as CFTypeRef) };
    }
    if merged.is_null() { None } else { Some(merged) }
}

/// One decoded channel from a delta sample.
struct ChannelItem {
    group: String,
    channel: String,
    unit: String,
    item: CFDictionaryRef,
}

/// Iterate the channels of a delta-sample dictionary.
fn iter_sample(sample: CFDictionaryRef) -> Vec<ChannelItem> {
    let mut out = Vec::new();
    let Some(items) = dict_get(sample, "IOReportChannels") else {
        return out;
    };
    let items = items as CFArrayRef;
    let n = unsafe { CFArrayGetCount(items) };
    for i in 0..n {
        let item = unsafe { CFArrayGetValueAtIndex(items, i) } as CFDictionaryRef;
        if item.is_null() {
            continue;
        }
        let group = from_cfstr(unsafe { IOReportChannelGetGroup(item) });
        let _subgroup = from_cfstr(unsafe { IOReportChannelGetSubGroup(item) });
        let channel = from_cfstr(unsafe { IOReportChannelGetChannelName(item) });
        let unit = from_cfstr(unsafe { IOReportChannelGetUnitLabel(item) })
            .trim()
            .to_string();
        out.push(ChannelItem {
            group,
            channel,
            unit,
            item,
        });
    }
    out
}

/// Convert an energy-channel integer (in `unit`) plus interval -> watts.
fn energy_to_watts(item: CFDictionaryRef, unit: &str, dt_secs: f64) -> Option<f64> {
    let raw = unsafe { IOReportSimpleGetIntegerValue(item, 0) } as f64;
    if raw < 0. {
        return None;
    }
    let joules = match unit {
        "nJ" => raw / 1e9,
        "uJ" => raw / 1e6,
        "mJ" => raw / 1e3,
        "J" => raw,
        _ => return None,
    };
    Some(joules / dt_secs.max(1e-9))
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

pub struct IoReportSampler {
    subs: IOReportSubscriptionRef,
    chan: CFMutableDictionaryRef,
    /// Previous raw sample + the instant it was taken, for delta integration.
    prev: Option<(CFDictionaryRef, std::time::Instant)>,
}

impl IoReportSampler {
    /// Subscribe to the Energy Model + CPU/GPU stats groups. Returns `None` if
    /// IOReport could not provide any channels (degrades gracefully).
    pub fn new() -> Option<Self> {
        let groups = [
            ("Energy Model", None),
            ("CPU Stats", Some("CPU Complex Performance States")),
            ("CPU Stats", Some("CPU Core Performance States")),
            ("GPU Stats", Some("GPU Performance States")),
        ];
        let chan = build_channels(&groups)?;
        let mut out: CFMutableDictionaryRef =
            null::<core_foundation_sys::dictionary::__CFDictionary>() as *mut _;
        let subs =
            unsafe { IOReportCreateSubscription(null(), chan, &mut out as *mut _, 0, null()) };
        if subs.is_null() {
            unsafe { CFRelease(chan as CFTypeRef) };
            return None;
        }
        // `out` is an owned copy returned by the subscription; release it (we
        // keep `chan`, which is what the sample calls take).
        if !out.is_null() {
            unsafe { CFRelease(out as CFTypeRef) };
        }
        Some(Self {
            subs,
            chan,
            prev: None,
        })
    }

    fn raw_sample(&self) -> (CFDictionaryRef, std::time::Instant) {
        let s = unsafe { IOReportCreateSamples(self.subs, self.chan, null()) };
        (s, std::time::Instant::now())
    }
}

impl Sampler for IoReportSampler {
    fn name(&self) -> &'static str {
        "IOReport (Energy Model + CPU/GPU stats)"
    }
}

impl PowerSampler for IoReportSampler {
    fn tick(&mut self, _dt: Duration) -> Option<SocPower> {
        // Take a fresh sample and diff against the previous one. We measure our
        // own interval from the two sample timestamps rather than trusting the
        // caller's `dt`, so the energy->watts conversion is exact.
        let prev = match self.prev.take() {
            Some(p) => p,
            None => {
                // First call: prime the baseline, no delta yet.
                self.prev = Some(self.raw_sample());
                return None;
            }
        };
        let next = self.raw_sample();
        let dt_secs = next.1.duration_since(prev.1).as_secs_f64();

        let delta = unsafe { IOReportCreateSamplesDelta(prev.0, next.0, null()) };
        unsafe { CFRelease(prev.0 as CFTypeRef) };
        self.prev = Some(next);

        if delta.is_null() {
            return None;
        }

        let mut soc = SocPower::default();
        // Energy Model channels are collected before being summed: the group is
        // hierarchical, and we cannot tell a sub-component from a whole block
        // until we know which aggregate channels this chip publishes.
        let mut energy: Vec<(String, f64)> = Vec::new();
        // Accumulators for residency-weighted frequency (state channels carry
        // residency ticks per DVFS step; we approximate active freq as the
        // residency-weighted mean over the non-idle states).
        let mut cpu_res: Vec<(String, i64)> = Vec::new();
        let mut gpu_res: Vec<(String, i64)> = Vec::new();

        for ch in iter_sample(delta) {
            match ch.group.as_str() {
                "Energy Model" => {
                    if let Some(w) = energy_to_watts(ch.item, &ch.unit, dt_secs) {
                        energy.push((ch.channel.clone(), w));
                    }
                }
                "CPU Stats" => {
                    // Skip the complementary "*_IDLE" channels (they report
                    // NON_IDLE residency and would distort the busy fraction);
                    // use only one representative complex channel (PCPU/MCPU0).
                    if !ch.channel.ends_with("_IDLE")
                        && (ch.channel == "PCPU" || ch.channel == "MCPU0")
                    {
                        accumulate_states(ch.item, &mut cpu_res);
                    }
                }
                "GPU Stats" if !ch.channel.ends_with("_IDLE") => {
                    accumulate_states(ch.item, &mut gpu_res);
                }
                _ => {}
            }
        }

        if energy.is_empty() {
            unsafe { CFRelease(delta as CFTypeRef) };
            return None;
        }
        accumulate_energy(&energy, &mut soc);

        let (cpu_active, cpu_p) = summarize_states(&cpu_res);
        let (gpu_active, gpu_p) = summarize_states(&gpu_res);
        soc.cpu_active_residency = cpu_active;
        soc.cpu_top_pstate = cpu_p;
        soc.gpu_active_residency = gpu_active;
        soc.gpu_top_pstate = gpu_p;

        unsafe { CFRelease(delta as CFTypeRef) };
        Some(soc)
    }
}

/// Sum the Energy Model channels, counting each joule exactly once.
///
/// The group is a HIERARCHY, not a flat list. On an M5 Pro it publishes 364
/// channels covering the same energy at four granularities: per-core-per-DVFS
/// -state (`MCPU0DTL2b`), per-core (`MCPU0_3`), per-cluster (`MCPU0`, `MCPM0`)
/// and per-block (`CPU Energy`). Naively summing everything counts the CPU
/// three or four times over - which is exactly what made SoC totals read about
/// double the SMC's `PSTR` cross-check.
///
/// The rule is structural rather than a per-chip whitelist: when a chip
/// publishes an aggregate channel, its constituents are dropped. A chip that
/// publishes no `CPU Energy` channel still gets its per-cluster channels summed.
fn accumulate_energy(channels: &[(String, f64)], soc: &mut SocPower) {
    let published = |name: &str| channels.iter().any(|(c, _)| c == name);
    let cpu_aggregate = published("CPU Energy");
    let gpu_aggregate = published("GPU Energy");

    for (name, w) in channels {
        if is_subcomponent(name, cpu_aggregate, gpu_aggregate) {
            continue;
        }
        soc.total_w += w;
        classify_energy(name, *w, soc);
    }
}

/// True when this channel's energy is already counted inside a coarser one.
fn is_subcomponent(name: &str, cpu_aggregate: bool, gpu_aggregate: bool) -> bool {
    let n = name.to_ascii_uppercase();
    // Per-core-per-P-state detail channels: always a breakdown of something.
    if n.contains("DTL") {
        return true;
    }
    // SRAM rails sit inside their block's own total.
    if n.ends_with("_SRAM") {
        return true;
    }
    // Core, cluster and cluster-misc channels, when the whole-CPU block exists.
    if cpu_aggregate
        && n != "CPU ENERGY"
        && (n.starts_with("MCPU")
            || n.starts_with("PCPU")
            || n.starts_with("ECPU")
            || n.starts_with("PACC")
            || n.starts_with("EACC")
            || n.starts_with("MCPM")
            || n.starts_with("PCPM"))
    {
        return true;
    }
    // Coarse duplicate of "GPU Energy".
    if gpu_aggregate && n == "GPU" {
        return true;
    }
    false
}

/// Map an Energy Model channel name to a SocPower field. Names are chip-
/// specific FourCC-style codes, so we match case-insensitive substrings and
/// total each block. Returns the watts that were NOT attributed to a named
/// block (folded into `total_w` by the caller regardless).
///
/// GPU has two channels on M5 (`GPU` mJ + `GPU Energy` nJ); we count only
/// `GPU Energy` and skip the coarse `GPU` to avoid double counting.
fn classify_energy(channel: &str, w: f64, soc: &mut SocPower) {
    let c = channel.to_ascii_lowercase();
    if c.contains("disp") {
        // DISP / DISPEXT: the SoC's display pipeline for the internal and
        // external panels. The panel backlight is not on this rail.
        *soc.display_w.get_or_insert(0.0) += w;
    } else if c.contains("ane") {
        *soc.ane_w.get_or_insert(0.0) += w;
    } else if c.contains("gpu") {
        // "GPU Energy"
        *soc.gpu_w.get_or_insert(0.0) += w;
    } else if c.contains("dram") || c.contains("dcs") || c.contains("amcc") {
        *soc.dram_w.get_or_insert(0.0) += w;
    } else if c.contains("cpu") {
        // PCPU*/MCPU*/PCPM* etc.
        *soc.cpu_w.get_or_insert(0.0) += w;
    }
    // Everything else (DISP, ISP, AVE, MSR, FAB, PCIe...) contributes to total
    // only; it is real SoC draw but not one of the headline blocks.
}

/// Pull every (state-name, residency) pair out of a state channel, preserving
/// DVFS step order (index position is meaningful: higher = faster P-state).
fn accumulate_states(item: CFDictionaryRef, out: &mut Vec<(String, i64)>) {
    let count = unsafe { IOReportStateGetCount(item) };
    for i in 0..count {
        let name = from_cfstr(unsafe { IOReportStateGetNameForIndex(item, i) });
        let res = unsafe { IOReportStateGetResidency(item, i) };
        out.push((name, res));
    }
}

/// From DVFS state residencies, compute (active fraction, top active P-state).
///
/// State labels on this chip are voltage/pstate codes (`DOWN`/`IDLE`/`OFF` =
/// idle; `V0P19`, `P1`.. = active steps), with no MHz, so we report:
///   * active fraction = non-idle residency / total residency, and
///   * the highest active state INDEX that had non-zero residency (a 1-based
///     ordinal where higher = a faster DVFS step), which indicates whether the
///     cluster was pushed to its top P-states.
fn summarize_states(states: &[(String, i64)]) -> (Option<f64>, Option<u32>) {
    if states.is_empty() {
        return (None, None);
    }
    let total: i64 = states.iter().map(|(_, r)| *r).sum();
    if total <= 0 {
        return (None, None);
    }
    let mut active: i64 = 0;
    let mut top_idx: u32 = 0;
    for (idx, (name, res)) in states.iter().enumerate() {
        let n = name.to_ascii_uppercase();
        let is_idle = n.contains("IDLE") || n == "DOWN" || n == "OFF";
        if is_idle {
            continue;
        }
        active += *res;
        if *res > 0 {
            // Highest active DVFS step that was actually visited this interval.
            top_idx = top_idx.max(idx as u32 + 1);
        }
    }
    let active_frac = active as f64 / total as f64;
    let top = if top_idx > 0 { Some(top_idx) } else { None };
    (Some(active_frac), top)
}

impl Drop for IoReportSampler {
    fn drop(&mut self) {
        unsafe {
            if let Some((p, _)) = self.prev.take() {
                CFRelease(p as CFTypeRef);
            }
            CFRelease(self.chan as CFTypeRef);
            CFRelease(self.subs as CFTypeRef);
        }
    }
}
