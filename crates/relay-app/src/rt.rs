//! Real-time thread priority (§6 rule 9).

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn set_realtime() {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
    }
    // SAFETY: affects only the calling thread.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        tracing::warn!("could not raise thread QoS (rc={rc})");
    }
}

/// Windows: register the thread with MMCSS under the "Games" task.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub fn set_realtime() {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
    let mut index = 0u32;
    // SAFETY: affects only the calling thread; the handle lives until the
    // thread exits, which is what we want.
    if let Err(e) = unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Games"), &mut index) }
    {
        tracing::warn!("MMCSS registration failed: {e}");
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn set_realtime() {}
