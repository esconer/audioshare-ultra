//! Studio Design System & Vector UI Primitives for AudioShare Ultra Desktop.
//!
//! All icons are painted with `egui::Painter` so no platform font fallback can
//! ever produce tofu boxes. Kept in its own module so `main.rs` no longer
//! duplicates these helpers across the app.

use std::collections::VecDeque;
use std::ops::RangeInclusive;

use eframe::egui;
use qrcode::{Color as QrColor, QrCode};

/// Central studio palette, derived once per frame from the dark/light mode.
#[allow(dead_code)]
pub struct Palette {
    pub card_fill: egui::Color32,
    pub card_stroke: egui::Color32,
    pub inner_fill: egui::Color32,
    pub inner_stroke: egui::Color32,
    pub muted_text: egui::Color32,
    pub accent: egui::Color32,
    pub accent_border: egui::Color32,
    pub ok_bg: egui::Color32,
    pub ok_fg: egui::Color32,
    pub warn_bg: egui::Color32,
    pub warn_fg: egui::Color32,
    pub danger: egui::Color32,
}

impl Palette {
    pub fn for_mode(dark: bool) -> Self {
        if dark {
            Self {
                // Tonal layering: soft 8% white hairline rims instead of
                // heavy blue-gray outlines (calm, borderless studio look).
                card_fill: egui::Color32::from_rgb(16, 22, 36),
                card_stroke: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 20),
                inner_fill: egui::Color32::from_rgb(10, 14, 23),
                inner_stroke: egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                muted_text: egui::Color32::from_rgb(139, 154, 181),
                accent: egui::Color32::from_rgb(37, 99, 235),
                accent_border: egui::Color32::from_rgb(96, 165, 250),
                ok_bg: egui::Color32::from_rgb(20, 83, 45),
                ok_fg: egui::Color32::from_rgb(134, 239, 172),
                warn_bg: egui::Color32::from_rgb(66, 32, 6),
                warn_fg: egui::Color32::from_rgb(253, 224, 71),
                danger: egui::Color32::from_rgb(185, 28, 28),
            }
        } else {
            Self {
                card_fill: egui::Color32::WHITE,
                card_stroke: egui::Color32::from_rgb(214, 222, 234),
                inner_fill: egui::Color32::from_rgb(245, 248, 252),
                inner_stroke: egui::Color32::from_rgb(220, 226, 236),
                muted_text: egui::Color32::from_rgb(100, 116, 139),
                accent: egui::Color32::from_rgb(37, 99, 235),
                accent_border: egui::Color32::from_rgb(59, 130, 246),
                ok_bg: egui::Color32::from_rgb(220, 252, 231),
                ok_fg: egui::Color32::from_rgb(21, 128, 61),
                warn_bg: egui::Color32::from_rgb(254, 243, 199),
                warn_fg: egui::Color32::from_rgb(146, 64, 14),
                danger: egui::Color32::from_rgb(185, 28, 28),
            }
        }
    }
}

pub fn apply_studio_theme(ctx: &egui::Context, dark_mode: bool) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(16.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.visuals = if dark_mode {
        let mut v = egui::Visuals::dark();
        v.panel_fill = egui::Color32::from_rgb(7, 10, 16);
        v.window_fill = egui::Color32::from_rgb(16, 22, 36);
        v.faint_bg_color = egui::Color32::from_rgb(20, 25, 36);
        v.extreme_bg_color = egui::Color32::from_rgb(7, 9, 14);
        v.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
        v.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(147, 197, 253));
        v.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(32, 40, 56));
        v.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(24, 30, 43);
        v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(22, 28, 40);
        v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(42, 52, 70));
        v.widgets.inactive.rounding = egui::Rounding::same(7.0);
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(34, 43, 62);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(96, 165, 250));
        v.widgets.hovered.rounding = egui::Rounding::same(7.0);
        v.widgets.active.bg_fill = egui::Color32::from_rgb(37, 99, 235);
        v.widgets.active.rounding = egui::Rounding::same(7.0);
        v
    } else {
        let mut v = egui::Visuals::light();
        v.panel_fill = egui::Color32::from_rgb(241, 245, 249);
        v.window_fill = egui::Color32::WHITE;
        v.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
        v.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
        v.widgets.inactive.rounding = egui::Rounding::same(7.0);
        v.widgets.hovered.rounding = egui::Rounding::same(7.0);
        v.widgets.active.rounding = egui::Rounding::same(7.0);
        v
    };
    ctx.set_style(style);
}

// ---------------------------------------------------------------------------
// Vector Icon Painters (Zero OS Font / Tofu Risk)
// ---------------------------------------------------------------------------

pub fn paint_play_icon(painter: &egui::Painter, center: egui::Pos2, size: f32, color: egui::Color32) {
    let r = size * 0.5;
    let pts = vec![
        egui::pos2(center.x - r * 0.65, center.y - r * 0.85),
        egui::pos2(center.x + r * 0.95, center.y),
        egui::pos2(center.x - r * 0.65, center.y + r * 0.85),
    ];
    painter.add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

pub fn paint_stop_icon(painter: &egui::Painter, center: egui::Pos2, size: f32, color: egui::Color32) {
    let rect = egui::Rect::from_center_size(center, egui::vec2(size * 0.78, size * 0.78));
    painter.rect_filled(rect, 2.0, color);
}

pub fn paint_chevron(painter: &egui::Painter, center: egui::Pos2, open: bool, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.8_f32, color);
    if open {
        painter.line_segment(
            [
                egui::pos2(center.x - 4.0, center.y - 2.0),
                egui::pos2(center.x, center.y + 2.5),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(center.x, center.y + 2.5),
                egui::pos2(center.x + 4.0, center.y - 2.0),
            ],
            stroke,
        );
    } else {
        painter.line_segment(
            [
                egui::pos2(center.x - 2.0, center.y - 4.0),
                egui::pos2(center.x + 2.5, center.y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(center.x + 2.5, center.y),
                egui::pos2(center.x - 2.0, center.y + 4.0),
            ],
            stroke,
        );
    }
}

pub fn paint_lock_icon(painter: &egui::Painter, center: egui::Pos2, color: egui::Color32) {
    let body = egui::Rect::from_center_size(
        egui::pos2(center.x, center.y + 2.0),
        egui::vec2(8.5, 6.5),
    );
    painter.rect_filled(body, 1.5, color);
    let shackle = egui::Stroke::new(1.5_f32, color);
    painter.line_segment(
        [
            egui::pos2(center.x - 2.5, center.y - 1.0),
            egui::pos2(center.x - 2.5, center.y - 4.0),
        ],
        shackle,
    );
    painter.line_segment(
        [
            egui::pos2(center.x + 2.5, center.y - 1.0),
            egui::pos2(center.x + 2.5, center.y - 4.0),
        ],
        shackle,
    );
    painter.line_segment(
        [
            egui::pos2(center.x - 2.5, center.y - 4.0),
            egui::pos2(center.x + 2.5, center.y - 4.0),
        ],
        shackle,
    );
}

/// Primary Hero CTA Button with a painted vector Play/Stop glyph.
pub fn primary_cta_button(ui: &mut egui::Ui, is_active: bool, start_lbl: &str, stop_lbl: &str) -> bool {
    let desired = egui::vec2(198.0, 44.0);
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let base = if is_active {
        egui::Color32::from_rgb(185, 28, 28)
    } else {
        egui::Color32::from_rgb(22, 163, 74)
    };
    let fill = if resp.is_pointer_button_down_on() {
        base.linear_multiply(0.85)
    } else if resp.hovered() {
        egui::Color32::from_rgb(
            base.r().saturating_add(16),
            base.g().saturating_add(16),
            base.b().saturating_add(16),
        )
    } else {
        base
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 9.0, fill);
    let icon_center = egui::pos2(rect.left() + 24.0, rect.center().y);
    if is_active {
        paint_stop_icon(painter, icon_center, 11.0, egui::Color32::WHITE);
    } else {
        paint_play_icon(painter, icon_center, 12.0, egui::Color32::WHITE);
    }
    let label = if is_active { stop_lbl } else { start_lbl };
    painter.text(
        egui::pos2(rect.left() + 40.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.5),
        egui::Color32::WHITE,
    );
    resp.clicked()
}

// ---------------------------------------------------------------------------
// Containers, Pills, Switches, & Cards
// ---------------------------------------------------------------------------

pub fn studio_card<R>(
    ui: &mut egui::Ui,
    dark_mode: bool,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let p = Palette::for_mode(dark_mode);
    egui::Frame::none()
        .fill(p.card_fill)
        .stroke(egui::Stroke::new(1.0_f32, p.card_stroke))
        .rounding(egui::Rounding::same(16.0))
        .inner_margin(egui::Margin::same(18.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add_contents(ui)
        })
}

/// Atmospheric Hero Card with a subtle audio-reactive ambient glow behind the
/// CTA. Uses concentric painted circles rather than a shader, so it costs
/// nothing when the stream is idle.
pub fn hero_stage_card<R>(
    ui: &mut egui::Ui,
    dark_mode: bool,
    is_live: bool,
    vu_pct: u8,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let p = Palette::for_mode(dark_mode);
    egui::Frame::none()
        .fill(p.card_fill)
        .stroke(egui::Stroke::new(
            1.0_f32,
            if is_live {
                egui::Color32::from_rgba_unmultiplied(16, 185, 129, 75)
            } else {
                p.card_stroke
            },
        ))
        .rounding(egui::Rounding::same(18.0))
        .inner_margin(egui::Margin::same(20.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            if dark_mode && is_live {
                let rect = ui.max_rect();
                let origin = egui::pos2(rect.left() + 95.0, rect.top() + 26.0);
                let boost = (vu_pct as f32 / 100.0) * 14.0;
                let painter = ui.painter();
                for (radius, alpha) in [(170.0, 6u8), (125.0, 10), (85.0, 14), (50.0, 18)] {
                    painter.circle_filled(
                        origin,
                        radius + boost,
                        egui::Color32::from_rgba_unmultiplied(16, 185, 129, alpha),
                    );
                }
            }
            add_contents(ui)
        })
}

/// Sleek macOS/Linear-style segmented quality selector with a smooth animated
/// pill indicator. Returns the newly selected index on click.
pub fn segmented_mode_bar(
    ui: &mut egui::Ui,
    dark_mode: bool,
    selected_idx: usize,
    items: &[(&str, &str)],
) -> Option<usize> {
    let p = Palette::for_mode(dark_mode);
    let mut clicked = None;
    egui::Frame::none()
        .fill(p.inner_fill)
        .stroke(egui::Stroke::new(1.0_f32, p.inner_stroke))
        .rounding(egui::Rounding::same(12.0))
        .inner_margin(egui::Margin::same(4.0))
        .show(ui, |ui| {
            let total_w = ui.available_width();
            let seg_w = total_w / (items.len().max(1) as f32);
            let (full_rect, _) =
                ui.allocate_exact_size(egui::vec2(total_w, 46.0), egui::Sense::hover());
            let anim_idx = ui.ctx().animate_value_with_time(
                ui.make_persistent_id("mode_seg_anim"),
                selected_idx as f32,
                0.16,
            );
            let pill_rect = egui::Rect::from_min_size(
                egui::pos2(full_rect.left() + anim_idx * seg_w, full_rect.top()),
                egui::vec2(seg_w, full_rect.height()),
            );
            ui.painter().rect_filled(
                pill_rect.shrink(2.0),
                9.0,
                if dark_mode {
                    egui::Color32::from_rgb(30, 58, 138)
                } else {
                    egui::Color32::from_rgb(219, 234, 254)
                },
            );
            for (idx, (title, sub)) in items.iter().enumerate() {
                let r = egui::Rect::from_min_size(
                    egui::pos2(full_rect.left() + idx as f32 * seg_w, full_rect.top()),
                    egui::vec2(seg_w, full_rect.height()),
                );
                if ui
                    .interact(r, ui.id().with(("seg", idx)), egui::Sense::click())
                    .clicked()
                {
                    clicked = Some(idx);
                }
                let is_sel = idx == selected_idx;
                let title_col = if is_sel {
                    if dark_mode {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_rgb(30, 58, 138)
                    }
                } else if dark_mode {
                    egui::Color32::from_rgb(203, 213, 225)
                } else {
                    egui::Color32::from_rgb(30, 41, 59)
                };
                ui.painter().text(
                    egui::pos2(r.center().x, r.top() + 14.0),
                    egui::Align2::CENTER_CENTER,
                    *title,
                    egui::FontId::proportional(13.0),
                    title_col,
                );
                ui.painter().text(
                    egui::pos2(r.center().x, r.top() + 32.0),
                    egui::Align2::CENTER_CENTER,
                    *sub,
                    egui::FontId::proportional(10.5),
                    if is_sel {
                        egui::Color32::from_rgb(191, 219, 254)
                    } else {
                        p.muted_text
                    },
                );
            }
        });
    clicked
}

pub fn status_pill(ui: &mut egui::Ui, text: &str, bg: egui::Color32, fg: egui::Color32) {
    egui::Frame::none()
        .fill(bg)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(9.0, 4.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(fg).size(11.5).strong());
        });
}

pub fn metric_tile(ui: &mut egui::Ui, dark_mode: bool, label: &str, value: &str) {
    let p = Palette::for_mode(dark_mode);
    egui::Frame::none()
        .fill(p.inner_fill)
        .stroke(egui::Stroke::new(1.0_f32, p.inner_stroke))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(11.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(egui::RichText::new(label).size(11.0).color(p.muted_text));
                ui.monospace(egui::RichText::new(value).size(13.0).strong());
            });
        });
}

pub fn pill_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let desired_size = egui::vec2(38.0, 20.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool(response.id, *on);
        let bg_off = egui::Color32::from_rgb(45, 55, 72);
        let bg_on = egui::Color32::from_rgb(37, 99, 235);
        let bg = egui::Color32::from_rgb(
            (bg_off.r() as f32 + (bg_on.r() as f32 - bg_off.r() as f32) * how_on) as u8,
            (bg_off.g() as f32 + (bg_on.g() as f32 - bg_off.g() as f32) * how_on) as u8,
            (bg_off.b() as f32 + (bg_on.b() as f32 - bg_off.b() as f32) * how_on) as u8,
        );
        let radius = 0.5 * rect.height();
        ui.painter().rect_filled(rect, radius, bg);
        let circle_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter().circle_filled(
            egui::pos2(circle_x, rect.center().y),
            radius - 2.5,
            egui::Color32::WHITE,
        );
    }
    response
}

pub fn setting_switch_row(
    ui: &mut egui::Ui,
    dark_mode: bool,
    title: &str,
    subtitle: &str,
    value: &mut bool,
) -> bool {
    let p = Palette::for_mode(dark_mode);
    let mut changed = false;
    egui::Frame::none()
        .fill(p.inner_fill)
        .stroke(egui::Stroke::new(1.0_f32, p.inner_stroke))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.5;
                    ui.label(egui::RichText::new(title).size(13.0).strong());
                    ui.label(egui::RichText::new(subtitle).size(11.0).color(p.muted_text));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if pill_switch(ui, value).changed() {
                        changed = true;
                    }
                });
            });
        });
    changed
}

pub fn pill_choice(ui: &mut egui::Ui, dark_mode: bool, selected: bool, label: &str) -> bool {
    let (fill, stroke, text_col) = if selected {
        (
            egui::Color32::from_rgb(37, 99, 235),
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(96, 165, 250)),
            egui::Color32::WHITE,
        )
    } else if dark_mode {
        (
            egui::Color32::from_rgb(24, 30, 43),
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(44, 54, 74)),
            egui::Color32::from_rgb(203, 213, 225),
        )
    } else {
        (
            egui::Color32::from_rgb(238, 242, 248),
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(203, 213, 225)),
            egui::Color32::from_rgb(30, 41, 59),
        )
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(12.5).strong().color(text_col))
            .fill(fill)
            .stroke(stroke)
            .rounding(egui::Rounding::same(7.0)),
    )
    .clicked()
}

#[allow(dead_code)]
pub fn mode_preset_card(
    ui: &mut egui::Ui,
    dark_mode: bool,
    selected: bool,
    title: &str,
    subtitle: &str,
    badge: &str,
) -> bool {
    let p = Palette::for_mode(dark_mode);
    let (fill, stroke, title_col) = if selected {
        (
            if dark_mode {
                egui::Color32::from_rgb(23, 37, 68)
            } else {
                egui::Color32::from_rgb(219, 234, 254)
            },
            egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(59, 130, 246)),
            if dark_mode {
                egui::Color32::WHITE
            } else {
                egui::Color32::from_rgb(30, 58, 138)
            },
        )
    } else {
        (
            p.inner_fill,
            egui::Stroke::new(1.0_f32, p.card_stroke),
            if dark_mode {
                egui::Color32::from_rgb(226, 232, 240)
            } else {
                egui::Color32::from_rgb(15, 23, 42)
            },
        )
    };

    let resp = egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    let dot_col = if selected {
                        egui::Color32::from_rgb(59, 130, 246)
                    } else {
                        egui::Color32::from_rgb(100, 116, 139)
                    };
                    ui.painter().circle_filled(dot_rect.center(), 4.0, dot_col);
                    ui.label(egui::RichText::new(title).size(13.5).strong().color(title_col));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        status_pill(
                            ui,
                            badge,
                            if selected {
                                egui::Color32::from_rgb(37, 99, 235)
                            } else {
                                egui::Color32::from_rgb(26, 33, 48)
                            },
                            if selected {
                                egui::Color32::WHITE
                            } else {
                                egui::Color32::from_rgb(148, 163, 184)
                            },
                        );
                    });
                });
                ui.label(egui::RichText::new(subtitle).size(11.0).color(p.muted_text));
            });
        })
        .response;

    ui.interact(resp.rect, resp.id.with("mode_click"), egui::Sense::click())
        .clicked()
}

pub fn drawer_header(
    ui: &mut egui::Ui,
    dark_mode: bool,
    open: &mut bool,
    title: &str,
    summary_pills: &[&str],
) {
    let p = Palette::for_mode(dark_mode);
    let bg = if *open {
        if dark_mode {
            egui::Color32::from_rgb(22, 29, 44)
        } else {
            egui::Color32::from_rgb(235, 241, 250)
        }
    } else {
        p.card_fill
    };
    let stroke = if *open {
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(59, 130, 246))
    } else {
        egui::Stroke::new(1.0_f32, p.card_stroke)
    };

    let resp = egui::Frame::none()
        .fill(bg)
        .stroke(stroke)
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(14.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (chev_rect, _) =
                    ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                paint_chevron(
                    ui.painter(),
                    chev_rect.center(),
                    *open,
                    egui::Color32::from_rgb(147, 197, 253),
                );
                ui.label(egui::RichText::new(title).size(13.5).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for &pill in summary_pills.iter().rev() {
                        status_pill(
                            ui,
                            pill,
                            egui::Color32::from_rgb(26, 34, 49),
                            egui::Color32::from_rgb(147, 197, 253),
                        );
                    }
                });
            });
        })
        .response;

    if ui
        .interact(resp.rect, resp.id.with("drawer_toggle"), egui::Sense::click())
        .clicked()
    {
        *open = !*open;
    }
}

/// 24-segment studio LED VU meter with a decaying white peak-hold indicator.
pub fn draw_led_vu_meter(ui: &mut egui::Ui, vu_pct: u8, peak_hold_pct: f32) {
    let segments = 24usize;
    let desired = egui::vec2(216.0, 16.0);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(10, 13, 20));

    let gap = 2.0f32;
    let seg_w = (rect.width() - gap * (segments as f32 + 1.0)) / (segments as f32);
    let active_segs = ((vu_pct as usize) * segments).div_ceil(100);
    let hold_seg = if peak_hold_pct > 2.0 {
        Some(
            (((peak_hold_pct as usize) * segments)
                .div_ceil(100))
            .saturating_sub(1)
            .min(segments - 1),
        )
    } else {
        None
    };

    for i in 0..segments {
        let x = rect.left() + gap + (i as f32) * (seg_w + gap);
        let seg_rect = egui::Rect::from_min_size(
            egui::pos2(x, rect.top() + 2.5),
            egui::vec2(seg_w, rect.height() - 5.0),
        );
        let color = if hold_seg == Some(i) && i >= active_segs {
            egui::Color32::from_rgb(241, 245, 249) // Crisp peak-hold LED
        } else if i < active_segs {
            if i >= 21 {
                egui::Color32::from_rgb(239, 68, 68)
            } else if i >= 16 {
                egui::Color32::from_rgb(245, 158, 11)
            } else {
                egui::Color32::from_rgb(16, 185, 129)
            }
        } else {
            egui::Color32::from_rgb(26, 33, 46)
        };
        painter.rect_filled(seg_rect, 1.5, color);
    }
}

pub fn studio_fader_row(
    ui: &mut egui::Ui,
    dark_mode: bool,
    title: &str,
    freq_badge: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
) -> bool {
    let mut changed = false;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).size(12.5).strong());
            if !freq_badge.is_empty() {
                status_pill(
                    ui,
                    freq_badge,
                    if dark_mode {
                        egui::Color32::from_rgb(24, 31, 46)
                    } else {
                        egui::Color32::from_rgb(230, 236, 245)
                    },
                    egui::Color32::from_rgb(130, 144, 168),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let is_boosted = value.abs() >= 0.05;
                let (bg, fg) = if is_boosted {
                    (
                        egui::Color32::from_rgb(30, 58, 138),
                        egui::Color32::from_rgb(147, 197, 253),
                    )
                } else {
                    (
                        egui::Color32::from_rgb(24, 30, 43),
                        egui::Color32::from_rgb(148, 163, 184),
                    )
                };
                let db_btn = egui::Button::new(
                    egui::RichText::new(format!("{:+.1} dB", *value))
                        .monospace()
                        .size(11.5)
                        .strong()
                        .color(fg),
                )
                .fill(bg)
                .rounding(egui::Rounding::same(6.0));
                if ui
                    .add(db_btn)
                    .on_hover_text("Click to reset band to 0.0 dB")
                    .clicked()
                {
                    *value = 0.0;
                    changed = true;
                }
            });
        });
        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(140.0);
        if ui
            .add(egui::Slider::new(value, range).show_value(false).trailing_fill(true))
            .changed()
        {
            changed = true;
        }
    });
    ui.add_space(2.0);
    changed
}

/// Clock-drift oscilloscope with reference gridlines and +/-500 ppm scale labels.
pub fn draw_drift_oscilloscope(ui: &mut egui::Ui, history: &VecDeque<f32>) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 64.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, egui::Color32::from_rgb(10, 13, 20));
    painter.rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(30, 38, 54)),
    );

    let mid_y = rect.center().y;
    let grid_col = egui::Color32::from_rgb(30, 40, 58);
    for frac in [-0.5f32, 0.0, 0.5] {
        let y = mid_y - frac * (rect.height() * 0.84);
        painter.line_segment(
            [
                egui::pos2(rect.left() + 4.0, y),
                egui::pos2(rect.right() - 4.0, y),
            ],
            egui::Stroke::new(1.0_f32, grid_col),
        );
    }
    let lbl_col = egui::Color32::from_rgb(100, 116, 139);
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        "+500 ppm",
        egui::FontId::monospace(9.5),
        lbl_col,
    );
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.bottom() - 4.0),
        egui::Align2::LEFT_BOTTOM,
        "-500 ppm",
        egui::FontId::monospace(9.5),
        lbl_col,
    );

    let step_x = rect.width() / (history.len().max(2) as f32 - 1.0);
    let points: Vec<egui::Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &ppm)| {
            let x = rect.left() + (i as f32) * step_x;
            let norm_y = (ppm / 500.0).clamp(-1.0, 1.0);
            egui::pos2(x, mid_y - norm_y * (rect.height() * 0.42))
        })
        .collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.75_f32, egui::Color32::from_rgb(56, 189, 248)),
    ));
}

pub fn draw_qr_code(ui: &mut egui::Ui, uri: &str) {
    let Ok(code) = QrCode::new(uri.as_bytes()) else {
        return;
    };
    let width = code.width();
    let module_px = 5.0f32;
    let quiet_zone = 4;
    let size_px = (width + quiet_zone * 2) as f32 * module_px;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size_px, size_px), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 8.0, egui::Color32::WHITE);
    for y in 0..width {
        for x in 0..width {
            if code[(x, y)] == QrColor::Dark {
                let min = rect.min
                    + egui::vec2(
                        (x + quiet_zone) as f32 * module_px,
                        (y + quiet_zone) as f32 * module_px,
                    );
                painter.rect_filled(
                    egui::Rect::from_min_size(min, egui::vec2(module_px, module_px)),
                    0.0,
                    egui::Color32::BLACK,
                );
            }
        }
    }
}
