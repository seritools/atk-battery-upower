//! Prints every input report from the vendor interfaces, to find unsolicited ones.

use std::collections::HashSet;
use std::thread;
use std::time::Instant;

use hidapi::HidApi;

fn main() {
    let api = HidApi::new().expect("hidapi init");
    let start = Instant::now();
    let mut seen = HashSet::new();
    let handles: Vec<_> = api
        .device_list()
        .filter(|i| i.vendor_id() == atk_battery::VID && i.usage_page() >= 0xff00)
        .filter(|i| seen.insert(i.path().to_owned()))
        .filter_map(|i| {
            let dev = api.open_path(i.path()).ok()?;
            let label = format!("{:04x} {}", i.product_id(), i.path().to_string_lossy());
            eprintln!("listening on {label}");
            Some(thread::spawn(move || {
                let mut buf = [0u8; 64];
                loop {
                    match dev.read(&mut buf) {
                        Ok(n) => println!(
                            "{:8.3}s {label}: {:02x?}",
                            start.elapsed().as_secs_f32(),
                            &buf[..n]
                        ),
                        Err(e) => {
                            println!("{label}: {e}");
                            return;
                        }
                    }
                }
            }))
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
}
