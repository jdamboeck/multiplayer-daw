//! Colours shared by the custom-painted views.

use egui::Color32;

/// Track colours, as `0xRRGGBB`.
pub const PALETTE: [u32; 8] = [
    0x4FA3E0, 0xE0794F, 0x6FCF7C, 0xD9B44A, 0xB07FE0, 0xE06FA8, 0x4FD0C5, 0xA0A8B4,
];

pub const CANVAS: Color32 = Color32::from_rgb(24, 25, 28);
pub const LANE: Color32 = Color32::from_rgb(31, 32, 36);
pub const LANE_ALT: Color32 = Color32::from_rgb(28, 29, 33);
pub const LANE_SELECTED: Color32 = Color32::from_rgb(38, 40, 46);
pub const RULER: Color32 = Color32::from_rgb(36, 37, 42);
pub const HEADER: Color32 = Color32::from_rgb(34, 35, 40);
pub const HEADER_SELECTED: Color32 = Color32::from_rgb(46, 49, 58);
pub const GRID_BAR: Color32 = Color32::from_rgba_premultiplied(44, 44, 44, 44);
pub const GRID_BEAT: Color32 = Color32::from_rgba_premultiplied(22, 22, 22, 22);
pub const GRID_SUB: Color32 = Color32::from_rgba_premultiplied(9, 9, 9, 9);
pub const TEXT: Color32 = Color32::from_rgb(214, 216, 222);
pub const TEXT_DIM: Color32 = Color32::from_rgb(140, 144, 154);
pub const PLAYHEAD: Color32 = Color32::from_rgb(255, 255, 255);
pub const LOOP: Color32 = Color32::from_rgb(92, 150, 230);
pub const SELECTION: Color32 = Color32::from_rgb(255, 255, 255);
pub const KEY_WHITE: Color32 = Color32::from_rgb(205, 207, 212);
pub const KEY_BLACK: Color32 = Color32::from_rgb(48, 49, 54);
pub const ROW_BLACK: Color32 = Color32::from_rgb(26, 27, 30);
pub const OUTSIDE: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 110);
pub const METER: Color32 = Color32::from_rgb(110, 205, 125);
pub const METER_HOT: Color32 = Color32::from_rgb(230, 90, 80);
pub const WARN: Color32 = Color32::from_rgb(226, 178, 84);

pub fn rgb(color: u32) -> Color32 {
    Color32::from_rgb((color >> 16) as u8, (color >> 8) as u8, color as u8)
}

/// `a` moved towards `b` by `t` (0 gives `a`, 1 gives `b`).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}
