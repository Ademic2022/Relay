//! Prints the system cursor from a background thread, as the host does.
fn main() {
    #[cfg(target_os = "macos")]
    std::thread::spawn(|| {
        for _ in 0..3 {
            let img = relay_capture::macos_cursor::current_image();
            let pos = relay_capture::macos_cursor::location();
            match img {
                Some(i) => println!(
                    "cursor {}x{} hot=({},{}) scale={} hash={:x} pos={pos:?} opaque_px={}",
                    i.width,
                    i.height,
                    i.hot_x,
                    i.hot_y,
                    i.scale_milli,
                    i.hash,
                    i.rgba.chunks(4).filter(|p| p[3] > 0).count()
                ),
                None => println!("no cursor image; pos={pos:?}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    })
    .join()
    .unwrap();
}
