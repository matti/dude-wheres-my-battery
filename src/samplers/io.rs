//! System-wide I/O throughput: disk via IOKit, network via sysctl. Sudoless.
//!
//! Disk: iterate every `IOBlockStorageDriver` IOService, read its nested
//! `Statistics` dict (`Bytes (Read)/(Write)`, `Operations (Read)/(Write)`),
//! and sum. Cumulative counters; we diff against the previous tick -> MB/s and
//! IOPS.
//!
//! Network: `sysctl(CTL_NET, PF_ROUTE, 0, 0, NET_RT_IFLIST2, 0)` returns a
//! sequence of `if_msghdr2` records, each carrying `if_data64.ifi_ibytes /
//! ifi_obytes` per interface. We sum all real interfaces and diff -> rx/tx B/s.
//! The loopback interface (lo0) is excluded so local traffic is not counted.

use std::time::Duration;

use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching, kIOMasterPortDefault,
};

use crate::sampler::Sampler;
use crate::samplers::cf::CFProps;
use crate::types::IoFrame;

// ---------------------------------------------------------------------------
// Cumulative counters captured each tick.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Counters {
    disk_read: u64,
    disk_write: u64,
    disk_ops: u64,
    net_rx: u64,
    net_tx: u64,
}

pub struct IoSampler {
    prev: Option<Counters>,
}

impl IoSampler {
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Sample current cumulative counters and diff against the previous tick.
    /// `dt` is the elapsed wall time. The first call primes the baseline and
    /// returns `None`.
    pub fn tick(&mut self, dt: Duration) -> Option<IoFrame> {
        let cur = Counters {
            disk_read: 0,
            disk_write: 0,
            disk_ops: 0,
            net_rx: 0,
            net_tx: 0,
        };
        let cur = read_counters(cur);
        let prev = self.prev.replace(cur)?;
        let dt_s = dt.as_secs_f64().max(1e-9);

        Some(IoFrame {
            disk_read_bps: cur.disk_read.saturating_sub(prev.disk_read) as f64 / dt_s,
            disk_write_bps: cur.disk_write.saturating_sub(prev.disk_write) as f64 / dt_s,
            disk_iops: cur.disk_ops.saturating_sub(prev.disk_ops) as f64 / dt_s,
            net_rx_bps: cur.net_rx.saturating_sub(prev.net_rx) as f64 / dt_s,
            net_tx_bps: cur.net_tx.saturating_sub(prev.net_tx) as f64 / dt_s,
        })
    }
}

impl Sampler for IoSampler {
    fn name(&self) -> &'static str {
        "IOBlockStorageDriver + NET_RT_IFLIST2"
    }
}

/// Fill a `Counters` with the current cumulative disk + network totals.
fn read_counters(mut c: Counters) -> Counters {
    read_disk(&mut c);
    read_net(&mut c);
    c
}

// ---------------------------------------------------------------------------
// Disk: sum IOBlockStorageDriver "Statistics".
// ---------------------------------------------------------------------------

fn read_disk(c: &mut Counters) {
    let matching = unsafe { IOServiceMatching(c"IOBlockStorageDriver".as_ptr()) };
    if matching.is_null() {
        return;
    }
    let mut iter: u32 = 0;
    let rc = unsafe { IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) };
    if rc != 0 {
        return;
    }
    loop {
        let service = unsafe { IOIteratorNext(iter) };
        if service == 0 {
            break;
        }
        // Copy this driver's whole property dict, then read the nested
        // "Statistics" sub-dict via the shared CFProps reader.
        let mut dict: CFDictionaryRef = std::ptr::null();
        let rc = unsafe {
            IORegistryEntryCreateCFProperties(
                service,
                &mut dict as *mut CFDictionaryRef as *mut CFMutableDictionaryRef,
                kCFAllocatorDefault,
                0,
            )
        };
        unsafe { IOObjectRelease(service) };
        if rc != 0 || dict.is_null() {
            continue;
        }
        let props = CFProps::from_owned_dict(dict);
        if let Some(stats) = props.dict("Statistics") {
            c.disk_read += stats.i64("Bytes (Read)").unwrap_or(0).max(0) as u64;
            c.disk_write += stats.i64("Bytes (Write)").unwrap_or(0).max(0) as u64;
            c.disk_ops += stats.i64("Operations (Read)").unwrap_or(0).max(0) as u64;
            c.disk_ops += stats.i64("Operations (Write)").unwrap_or(0).max(0) as u64;
        }
    }
    unsafe { IOObjectRelease(iter) };
}

// ---------------------------------------------------------------------------
// Network: sum if_data64 byte counters from NET_RT_IFLIST2.
// ---------------------------------------------------------------------------

fn read_net(c: &mut Counters) {
    let mut mib: [libc::c_int; 6] = [
        libc::CTL_NET,
        libc::PF_ROUTE,
        0,
        0, // address family: 0 = all
        libc::NET_RT_IFLIST2,
        0,
    ];

    let mut len: libc::size_t = 0;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            6,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return;
    }
    let mut buf = vec![0u8; len];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            6,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return;
    }
    buf.truncate(len);

    // Walk the variable-length message stream by ifm_msglen.
    let mut off = 0usize;
    let hdr_size = std::mem::size_of::<libc::if_msghdr2>();
    while off + hdr_size <= buf.len() {
        // SAFETY: bounds checked above; if_msghdr2 is POD. Read unaligned since
        // the message stream is byte-packed.
        let hdr =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::if_msghdr2) };
        let msglen = hdr.ifm_msglen as usize;
        if msglen == 0 {
            break;
        }
        if hdr.ifm_type as libc::c_int == libc::RTM_IFINFO2 {
            // Exclude loopback (IFT_LOOP = 0x18) so local traffic isn't counted.
            const IFT_LOOP: u8 = 0x18;
            if hdr.ifm_data.ifi_type != IFT_LOOP {
                c.net_rx += hdr.ifm_data.ifi_ibytes;
                c.net_tx += hdr.ifm_data.ifi_obytes;
            }
        }
        off += msglen;
    }
}
