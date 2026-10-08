//! Manual check of window border colours:
//! `cargo run -p rivewm-platform --example border -- <hwnd> <rrggbb|default|none|get>`

use std::ffi::c_void;

use rivewm_core::WindowId;
use rivewm_platform::BorderColor;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_BORDER_COLOR, DwmGetWindowAttribute};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [hwnd, color] = &args[..] else {
        eprintln!("usage: border <hwnd> <rrggbb|default|none|get>");
        std::process::exit(2);
    };
    let hwnd = isize::from_str_radix(hwnd.trim_start_matches("0x"), 16).expect("hex hwnd");
    let color = match color.as_str() {
        "get" => {
            let mut value: u32 = 0;
            let result = unsafe {
                DwmGetWindowAttribute(
                    HWND(hwnd as *mut c_void),
                    DWMWA_BORDER_COLOR,
                    &mut value as *mut u32 as *mut c_void,
                    size_of::<u32>() as u32,
                )
            };
            println!("{result:?} colorref={value:#010x}");
            return;
        }
        "default" => BorderColor::Default,
        "none" => BorderColor::Hidden,
        hex => {
            let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).expect("rrggbb");
            BorderColor::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
        }
    };
    println!(
        "{:?}",
        rivewm_platform::set_border_color(WindowId(hwnd), color)
    );
}
