//! Static host identity, read once at startup: the marketing chip name and the
//! machine model. Used to label the live view (and to make it obvious when
//! power is running on a chip it has never seen).

use std::ffi::CString;

/// Read a string `sysctl` by name, e.g. `hw.model` -> `"Mac17,8"`.
fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: passing a null buffer asks the kernel for the required length.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buf is `len` bytes, exactly what the sizing call asked for.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    // The value is NUL-terminated; drop the terminator and anything after it.
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// Marketing chip name, e.g. `"Apple M5 Pro"`.
pub fn chip() -> String {
    sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "unknown SoC".to_string())
}

/// Machine model identifier, e.g. `"Mac17,8"`.
pub fn model() -> String {
    sysctl_string("hw.model").unwrap_or_else(|| "unknown model".to_string())
}
