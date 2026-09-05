//! Battery / whole-system power sampler via IOKit's `AppleSmartBattery`.
//!
//! `IOServiceGetMatchingService(IOServiceMatching("AppleSmartBattery"))` then
//! `IORegistryEntryCreateCFProperties` gives a CFDictionary of pack telemetry.
//! The headline number is whole-system drain:
//!
//!   system_power_mW = Voltage(mV) * InstantAmperage(mA) / 1000
//!
//! Negative = discharging (on battery). The signedness trap: ioreg prints
//! `InstantAmperage`/`Amperage` as unsigned 64-bit, but they are really i64
//! (e.g. 18446744073709550955 == -661). The shared `CFProps::i64` reads them
//! as signed, which gives the correct sign.

use crate::sampler::{BatterySampler, Sampler};
use crate::samplers::cf::CFProps;
use crate::types::BatteryFrame;

pub struct AppleSmartBatterySampler;

impl AppleSmartBatterySampler {
    pub fn new() -> Self {
        Self
    }
}

impl Sampler for AppleSmartBatterySampler {
    fn name(&self) -> &'static str {
        "AppleSmartBattery"
    }
}

impl BatterySampler for AppleSmartBatterySampler {
    fn read(&mut self) -> Option<BatteryFrame> {
        let props = CFProps::for_service("AppleSmartBattery")?;

        let voltage_mv = props.f64("Voltage")?;
        // InstantAmperage must be read as signed i64 to get the discharge sign.
        let instant_ma = props
            .i64("InstantAmperage")
            .or_else(|| props.i64("Amperage"))? as f64;

        Some(BatteryFrame {
            system_power_mw: voltage_mv * instant_ma / 1000.0,
            voltage_mv,
            instant_amperage_ma: instant_ma,
            soc_percent: props.f64("CurrentCapacity").unwrap_or(0.0),
            temperature_c: props.f64("Temperature").unwrap_or(0.0) / 100.0,
            time_remaining_min: props.i64("TimeRemaining").filter(|m| *m > 0 && *m < 65535),
            is_charging: props.bool("IsCharging").unwrap_or(false),
            external_connected: props.bool("ExternalConnected").unwrap_or(false),
        })
    }
}
