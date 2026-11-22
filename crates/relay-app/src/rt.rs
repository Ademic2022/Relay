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

/// Windows: MMCSS "Games" via AvSetMmThreadCharacteristicsW, added with the
/// Windows backends.
#[cfg(not(target_os = "macos"))]
pub fn set_realtime() {}
