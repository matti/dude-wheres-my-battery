//! Thermal sampler: SMC die temperatures + macOS thermal pressure.
//!
//! Two sudoless sources, closing the "heat" half of power:
//!
//!   1. AppleSMC user client. Open the `AppleSMCKeysEndpoint` service via
//!      `IOServiceOpen`, then `IOConnectCallStructMethod(conn, 2, ...)` with a
//!      `KeyData` struct. The data8 selector picks the operation:
//!      8 = key-by-index, 9 = key-info, 5 = read-value.
//!      We enumerate every key at runtime (`#KEY` gives the count), keep the
//!      temperature keys whose FourCC starts with Tp/Te/Ts/Tg, and decode the
//!      `flt ` (f32 LE) values. NOTHING is hardcoded: Ts* (super-core) keys are
//!      M5+-only and the bank changes every chip generation. `result == 132` is
//!      key-not-found and is handled gracefully (this MBP has no fan keys).
//!
//!      Struct layout + selectors are transcribed from macmon's `sources.rs`
//!      (MIT) - the verified reference - not from memory.
//!
//!   2. Thermal pressure via notify(3): `notify_register_check` +
//!      `notify_get_state("com.apple.system.thermalpressurelevel")` -> 0..3.
//!
//! The SMC connection is opened once and reused; `key_info` is cached per key
//! so each tick is just selector-5 reads. `IOServiceClose` on drop.

use std::collections::HashMap;
use std::ffi::CString;

use io_kit_sys::{
    IOConnectCallStructMethod, IOIteratorNext, IOObjectRelease, IORegistryEntryGetName,
    IOServiceClose, IOServiceGetMatchingServices, IOServiceMatching, IOServiceOpen,
    kIOMasterPortDefault,
};
use mach2::traps::mach_task_self;

use crate::sampler::{Sampler, ThermalSampler};
use crate::types::{ThermalFrame, ThermalPressure};

// ---------------------------------------------------------------------------
// SMC FFI structs (layout per macmon sources.rs, verified reference).
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default)]
struct KeyDataVer {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Debug, Default)]
struct PLimitData {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

#[repr(C)]
#[derive(Debug, Default)]
struct KeyData {
    key: u32,
    vers: KeyDataVer,
    p_limit_data: PLimitData,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

unsafe extern "C" {
    // notify(3) from libSystem.
    fn notify_register_check(name: *const libc::c_char, out_token: *mut libc::c_int) -> u32;
    fn notify_get_state(token: libc::c_int, state64: *mut u64) -> u32;
}

/// FourCC `flt ` data type (little-endian f32).
const SMC_TYPE_FLT: u32 = u32::from_be_bytes(*b"flt ");
/// SMC `result` value meaning "key not found".
const SMC_KEY_NOT_FOUND: u8 = 132;

// ---------------------------------------------------------------------------
// SMC connection
// ---------------------------------------------------------------------------

struct Smc {
    conn: u32,
    key_info: HashMap<u32, KeyInfo>,
}

/// Encode a 4-char key as a big-endian FourCC u32.
fn fourcc(key: &str) -> u32 {
    key.bytes().fold(0u32, |acc, b| (acc << 8) | b as u32)
}

impl Smc {
    /// Open the AppleSMCKeysEndpoint user client. Returns `None` if unavailable.
    fn open() -> Option<Self> {
        let matching = unsafe { IOServiceMatching(c"AppleSMC".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        let mut iter: u32 = 0;
        let rc = unsafe { IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) };
        if rc != 0 {
            return None;
        }

        let mut conn: u32 = 0;
        loop {
            let service = unsafe { IOIteratorNext(iter) };
            if service == 0 {
                break;
            }
            let mut name = [0i8; 128];
            let ok = unsafe { IORegistryEntryGetName(service, name.as_mut_ptr()) };
            let is_endpoint = ok == 0
                && unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes()
                    == b"AppleSMCKeysEndpoint";
            if is_endpoint {
                let rs = unsafe { IOServiceOpen(service, mach_task_self(), 0, &mut conn) };
                unsafe { IOObjectRelease(service) };
                if rs == 0 {
                    break;
                }
                conn = 0;
            } else {
                unsafe { IOObjectRelease(service) };
            }
        }
        unsafe { IOObjectRelease(iter) };

        if conn == 0 {
            None
        } else {
            Some(Self {
                conn,
                key_info: HashMap::new(),
            })
        }
    }

    /// One `IOConnectCallStructMethod(conn, 2, ...)` round-trip.
    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut olen = std::mem::size_of::<KeyData>();
        let rc = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                2,
                input as *const KeyData as *const _,
                std::mem::size_of::<KeyData>(),
                &mut out as *mut KeyData as *mut _,
                &mut olen,
            )
        };
        if rc != 0 || out.result == SMC_KEY_NOT_FOUND || out.result != 0 {
            return None;
        }
        Some(out)
    }

    /// Total number of SMC keys (`#KEY`).
    fn key_count(&mut self) -> Option<u32> {
        let v = self.read_raw("#KEY")?;
        Some(u32::from_be_bytes(v.0[0..4].try_into().ok()?))
    }

    /// The 4-char key at a given index (data8 = 8).
    fn key_by_index(&self, index: u32) -> Option<String> {
        let input = KeyData {
            data8: 8,
            data32: index,
            ..Default::default()
        };
        let out = self.call(&input)?;
        let bytes = out.key.to_be_bytes();
        std::str::from_utf8(&bytes).ok().map(|s| s.to_string())
    }

    /// Key metadata (data8 = 9), cached.
    fn read_key_info(&mut self, key: &str) -> Option<KeyInfo> {
        let k = fourcc(key);
        if let Some(ki) = self.key_info.get(&k) {
            return Some(*ki);
        }
        let input = KeyData {
            data8: 9,
            key: k,
            ..Default::default()
        };
        let out = self.call(&input)?;
        self.key_info.insert(k, out.key_info);
        Some(out.key_info)
    }

    /// Raw value bytes + its data_type FourCC (data8 = 5).
    fn read_raw(&mut self, key: &str) -> Option<([u8; 32], u32)> {
        let ki = self.read_key_info(key)?;
        let input = KeyData {
            data8: 5,
            key: fourcc(key),
            key_info: ki,
            ..Default::default()
        };
        let out = self.call(&input)?;
        Some((out.bytes, ki.data_type))
    }

    /// Read a `flt ` key as f32. `None` if absent or not a float key.
    fn read_flt(&mut self, key: &str) -> Option<f64> {
        let ki = self.read_key_info(key)?;
        if ki.data_type != SMC_TYPE_FLT || ki.data_size < 4 {
            return None;
        }
        let (bytes, _) = self.read_raw(key)?;
        let v = f32::from_le_bytes(bytes[0..4].try_into().ok()?);
        if v.is_finite() { Some(v as f64) } else { None }
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        if self.conn != 0 {
            unsafe { IOServiceClose(self.conn) };
        }
    }
}

// ---------------------------------------------------------------------------
// Thermal pressure
// ---------------------------------------------------------------------------

fn read_thermal_pressure(token: libc::c_int) -> ThermalPressure {
    let mut state: u64 = 0;
    let rc = unsafe { notify_get_state(token, &mut state) };
    if rc != 0 {
        return ThermalPressure::Unknown(u64::MAX);
    }
    match state {
        0 => ThermalPressure::Nominal,
        // OSThermalPressureLevel uses non-contiguous values across releases;
        // map the documented levels and treat anything else as fair-or-worse.
        1 | 10 => ThermalPressure::Fair,
        2 | 20 => ThermalPressure::Serious,
        3 | 30 => ThermalPressure::Critical,
        other => ThermalPressure::Unknown(other),
    }
}

// ---------------------------------------------------------------------------
// Sampler
// ---------------------------------------------------------------------------

pub struct ThermalSamplerImpl {
    smc: Option<Smc>,
    /// Cached list of CPU (Tp/Te/Ts) and GPU (Tg) float temperature keys,
    /// discovered once via the runtime key enumeration.
    cpu_keys: Vec<String>,
    gpu_keys: Vec<String>,
    pressure_token: Option<libc::c_int>,
}

impl ThermalSamplerImpl {
    pub fn new() -> Self {
        let mut smc = Smc::open();
        let (cpu_keys, gpu_keys) = match smc.as_mut() {
            Some(s) => discover_temp_keys(s),
            None => (Vec::new(), Vec::new()),
        };

        // Register for the thermal pressure state.
        let name = CString::new("com.apple.system.thermalpressurelevel").unwrap();
        let mut token: libc::c_int = 0;
        let pressure_token = if unsafe { notify_register_check(name.as_ptr(), &mut token) } == 0 {
            Some(token)
        } else {
            None
        };

        Self {
            smc,
            cpu_keys,
            gpu_keys,
            pressure_token,
        }
    }
}

/// Enumerate all SMC keys once and collect the CPU / GPU float temperature
/// keys (prefixes Tp/Te/Ts for CPU, Tg for GPU). Only keeps keys that read
/// back as a sane temperature so we don't include unrelated Tx* sensors.
fn discover_temp_keys(smc: &mut Smc) -> (Vec<String>, Vec<String>) {
    let mut cpu = Vec::new();
    let mut gpu = Vec::new();
    let Some(count) = smc.key_count() else {
        return (cpu, gpu);
    };
    for i in 0..count {
        let Some(key) = smc.key_by_index(i) else {
            continue;
        };
        let prefix2 = &key[..key.len().min(2)];
        let (is_cpu, is_gpu) = match prefix2 {
            "Tp" | "Te" | "Ts" => (true, false),
            "Tg" => (false, true),
            _ => (false, false),
        };
        if !is_cpu && !is_gpu {
            continue;
        }
        // Must be a float key that reads a plausible on-die temperature.
        match smc.read_flt(&key) {
            Some(t) if (0.0..=130.0).contains(&t) => {
                if is_cpu {
                    cpu.push(key);
                } else {
                    gpu.push(key);
                }
            }
            _ => {}
        }
    }
    (cpu, gpu)
}

impl Sampler for ThermalSamplerImpl {
    fn name(&self) -> &'static str {
        "AppleSMC + thermalpressure"
    }
}

impl ThermalSampler for ThermalSamplerImpl {
    fn read(&mut self) -> ThermalFrame {
        let mut frame = ThermalFrame::default();

        if let Some(smc) = self.smc.as_mut() {
            // CPU die temps: max + mean.
            let cpu_temps: Vec<f64> = self
                .cpu_keys
                .iter()
                .filter_map(|k| smc.read_flt(k))
                .collect();
            if !cpu_temps.is_empty() {
                let max = cpu_temps.iter().cloned().fold(f64::MIN, f64::max);
                let mean = cpu_temps.iter().sum::<f64>() / cpu_temps.len() as f64;
                frame.cpu_die_max_c = Some(max);
                frame.cpu_die_mean_c = Some(mean);
            }

            // GPU die temps: max.
            let gpu_temps: Vec<f64> = self
                .gpu_keys
                .iter()
                .filter_map(|k| smc.read_flt(k))
                .collect();
            if !gpu_temps.is_empty() {
                frame.gpu_die_max_c = Some(gpu_temps.iter().cloned().fold(f64::MIN, f64::max));
            }

            frame.sensor_count = cpu_temps.len() + gpu_temps.len();

            // PSTR: total system power in watts (independent cross-check).
            frame.pstr_w = smc.read_flt("PSTR");
        }

        frame.pressure = match self.pressure_token {
            Some(t) => read_thermal_pressure(t),
            None => ThermalPressure::Unknown(u64::MAX),
        };

        frame
    }
}

/// Every float-typed SMC key with its current value, sorted by key.
///
/// Used by `power doctor` to answer "what can this machine actually
/// measure" without hardcoding anything per chip. Power keys are the `P*`
/// ones; `PSTR` is whole-system power, and what else exists varies by model.
pub fn dump_float_keys() -> Vec<(String, f64)> {
    let Some(mut smc) = Smc::open() else {
        return Vec::new();
    };
    let count = smc.key_count().unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..count {
        if let Some(key) = smc.key_by_index(i)
            && let Some(v) = smc.read_flt(&key)
        {
            out.push((key, v));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
