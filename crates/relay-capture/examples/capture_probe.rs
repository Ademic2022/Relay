//! Lists displays and captures for 3 s, printing the frame rate.
//! `cargo run -p relay-capture --example capture_probe`

use std::time::{Duration, Instant};

fn main() {
    let mut cap = match relay_capture::platform_capturer() {
        Ok(c) => c,
        Err(e) => return eprintln!("no capturer: {e}"),
    };
    let displays = cap.displays().expect("displays");
    for d in &displays {
        println!("{d:?}");
    }
    let id = displays
        .iter()
        .find(|d| d.primary)
        .unwrap_or(&displays[0])
        .id;
    cap.select(id).expect("select");
    let t0 = Instant::now();
    let (mut n, mut idle) = (0, 0);
    while t0.elapsed() < Duration::from_secs(3) {
        match cap.next_frame(Duration::from_millis(100)).expect("frame") {
            Some(f) => {
                n += 1;
                if n == 1 {
                    println!("first frame {}x{} {:?}", f.width, f.height, f.format);
                }
            }
            None => idle += 1,
        }
    }
    println!(
        "{n} frames in 3 s ({:.1} fps), {idle} idle waits",
        n as f64 / 3.0
    );
}
