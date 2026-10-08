//! Manual check that cloaking works on this Windows build:
//! `cargo run -p rivewm-platform --example cloak -- <hwnd> on|off`

use rivewm_core::WindowId;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [hwnd, state] = &args[..] else {
        eprintln!("usage: cloak <hwnd> on|off");
        std::process::exit(2);
    };
    let hwnd = isize::from_str_radix(hwnd.trim_start_matches("0x"), 16).expect("hex hwnd");
    let id = WindowId(hwnd);
    let result = rivewm_platform::set_cloaked(id, state == "on");
    println!(
        "set_cloaked({state}) -> {result:?}; now cloaked = {}",
        rivewm_platform::is_window_cloaked(id)
    );
}
