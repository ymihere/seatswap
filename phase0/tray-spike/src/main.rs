//! Phase 0 item 8 spike: does a Tauri 2 tray icon on Cinnamon update its
//! picture, its label and its menu text live? Runs for N seconds (arg 1).
use std::time::{Duration, Instant};
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};

const SIZE: u32 = 32;

/// Account-coloured disc with a usage ring from 12 o'clock, clockwise.
fn ring_icon(percent: u32) -> Image<'static> {
    let (w, c) = (SIZE as f32, SIZE as f32 / 2.0);
    let (outer, inner) = (w / 2.0 - 0.5, w / 2.0 - 4.5);
    let ring_rgb = match percent {
        0..=69 => [0x22, 0xc5, 0x5e],
        70..=89 => [0xf5, 0x9e, 0x0b],
        _ => [0xef, 0x44, 0x44],
    };
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let px = if d <= inner {
                [0x3b, 0x82, 0xf6, 255] // account colour
            } else if d <= outer {
                // angle from 12 o'clock, clockwise, 0..1
                let a = (dx.atan2(-dy) / std::f32::consts::TAU).rem_euclid(1.0);
                if a * 100.0 < percent as f32 {
                    [ring_rgb[0], ring_rgb[1], ring_rgb[2], 255]
                } else {
                    [0x55, 0x5b, 0x66, 255]
                }
            } else {
                [0, 0, 0, 0]
            };
            let i = ((y * SIZE + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&px);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}

fn main() {
    let run_for = Duration::from_secs(std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(40));
    tauri::Builder::default()
        .setup(move |app| {
            let account = MenuItem::with_id(app, "as", "AC  Acme   0 %", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&account, &quit])?;
            let tray = TrayIconBuilder::with_id("main")
                .icon(ring_icon(0))
                .menu(&menu)
                .title("0%")
                .tooltip("tray-spike")
                .on_menu_event(|app, ev| {
                    println!("menu click: {}", ev.id().as_ref());
                    if ev.id().as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .build(app)?;
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                let mut percent = 0u32;
                while start.elapsed() < run_for {
                    std::thread::sleep(Duration::from_millis(500));
                    percent = (percent + 5) % 105;
                    let t = Instant::now();
                    let r1 = tray.set_icon(Some(ring_icon(percent)));
                    let r2 = tray.set_title(Some(format!("{percent}%")));
                    let r3 = account.set_text(format!("AC  Acme   {percent} %"));
                    println!("{:>5.1}s  {percent:>3}%  icon={r1:?} title={r2:?} menu={r3:?}  ({} us)",
                             start.elapsed().as_secs_f32(), t.elapsed().as_micros());
                }
                handle.exit(0);
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("tauri run failed");
}
