//! One GPU snapshot per recording interval. Last submission is an identity
//! hint, not a utilization share; no watts are apportioned to that process.
use crate::samplers::{cf::CFProps, procname};
use crate::types::GpuFrame;

pub struct AgxSampler {
    frame: GpuFrame,
}
impl AgxSampler {
    pub fn new() -> Self {
        Self {
            frame: GpuFrame::default(),
        }
    }
    pub fn begin(&mut self) {
        self.frame = GpuFrame::default();
    }
    pub fn poll(&mut self) {
        let Some(props) = CFProps::for_service("IOAccelerator") else {
            return;
        };
        self.frame.total_samples = 1;
        if let Some(agc) = props.dict("AGCInfo")
            && let Some(pid) = agc
                .i64("fLastSubmissionPID")
                .filter(|pid| *pid > 0 && *pid <= i32::MAX as i64)
        {
            let pid = pid as i32;
            self.frame.top_submitter_pid = Some(pid);
            self.frame.top_submitter_name = Some(procname::resolve_name(pid));
            self.frame.top_submitter_hits = 1;
        }
        if let Some(perf) = props.dict("PerformanceStatistics") {
            self.frame.device_util_pct = perf.f64("Device Utilization %");
            self.frame.in_use_mem_bytes = perf.i64("In use system memory").map(|v| v as u64);
        }
    }
    pub fn finish(&mut self, _: Option<f64>) -> GpuFrame {
        std::mem::take(&mut self.frame)
    }
}
