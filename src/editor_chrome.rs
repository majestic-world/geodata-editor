//! Shared native editor palette and resolution-independent vector artwork.

use egui::{Color32, Pos2, Rect, Response, Stroke, Ui, Vec2};

use crate::{editor::EditorTheme, l2j::Direction};

pub(super) fn background(theme: EditorTheme) -> Color32 {
    match theme {
        EditorTheme::Dark => Color32::from_rgb(23, 25, 29),
        EditorTheme::Light => Color32::from_rgb(234, 237, 241),
    }
}

pub(super) fn accent(theme: EditorTheme) -> Color32 {
    match theme {
        EditorTheme::Dark => Color32::from_rgb(74, 158, 255),
        EditorTheme::Light => Color32::from_rgb(24, 101, 191),
    }
}

pub(super) fn apply_theme(context: &egui::Context, theme: EditorTheme) {
    let dark = theme == EditorTheme::Dark;
    let mut style = (*context.style()).clone();
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let rgb = Color32::from_rgb;
    let panel = if dark {
        rgb(34, 37, 43)
    } else {
        rgb(247, 248, 250)
    };
    let border = if dark {
        rgb(54, 59, 68)
    } else {
        rgb(200, 207, 217)
    };
    let text = if dark {
        rgb(222, 226, 233)
    } else {
        rgb(38, 46, 57)
    };
    style.visuals.panel_fill = panel;
    style.visuals.window_fill = panel;
    style.visuals.extreme_bg_color = background(theme);
    style.visuals.faint_bg_color = if dark {
        rgb(39, 43, 50)
    } else {
        rgb(229, 234, 241)
    };
    style.visuals.selection.bg_fill = if dark {
        rgb(35, 73, 118)
    } else {
        rgb(202, 225, 252)
    };
    style.visuals.selection.stroke = Stroke::new(1.0_f32, accent(theme));
    style.visuals.window_stroke = Stroke::new(1.0_f32, border);
    style.visuals.window_rounding = 4.0.into();
    style.visuals.menu_rounding = 4.0.into();
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, border);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, text);
    style.visuals.widgets.noninteractive.rounding = 3.0.into();
    for (widget, fill) in [
        (
            &mut style.visuals.widgets.inactive,
            if dark {
                rgb(46, 50, 58)
            } else {
                rgb(229, 234, 241)
            },
        ),
        (
            &mut style.visuals.widgets.hovered,
            if dark {
                rgb(62, 70, 82)
            } else {
                rgb(215, 228, 245)
            },
        ),
        (
            &mut style.visuals.widgets.active,
            if dark {
                rgb(39, 79, 128)
            } else {
                rgb(194, 217, 247)
            },
        ),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0_f32, border);
        widget.fg_stroke = Stroke::new(1.0_f32, text);
        widget.rounding = 3.0.into();
        widget.expansion = 0.0;
    }
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, accent(theme));
    style.visuals.widgets.hovered.bg_stroke =
        Stroke::new(1.0_f32, accent(theme).gamma_multiply(0.7));
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size = egui::vec2(64.0, 26.0);
    style.spacing.combo_width = 116.0;
    style.spacing.indent = 14.0;
    style.visuals.indent_has_left_vline = false;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(18.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
    context.set_style(style);
}

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Folder,
    Save,
    Undo,
    Redo,
    Layers,
    Cube,
    Grid,
    Eye,
    Sun,
    Compass,
}

/// Paths use a 24-unit viewbox, matching the Higgsfield concept's line icons.
pub(super) fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let point =
        |x: f32, y: f32| rect.min + egui::vec2(x / 24.0 * rect.width(), y / 24.0 * rect.height());
    let stroke = Stroke::new((rect.width() / 24.0 * 1.5).clamp(1.0, 2.0), color);
    let path = |points: &[[f32; 2]]| {
        painter.add(egui::Shape::line(
            points.iter().map(|p| point(p[0], p[1])).collect(),
            stroke,
        ));
    };
    match icon {
        Icon::Folder => {
            path(&[
                [3., 19.],
                [3., 5.],
                [9., 5.],
                [12., 8.],
                [20., 8.],
                [20., 11.],
            ]);
            path(&[[3., 19.], [6., 11.], [22., 11.], [19., 19.], [3., 19.]]);
        }
        Icon::Save => {
            path(&[
                [4., 3.],
                [17., 3.],
                [21., 7.],
                [21., 21.],
                [3., 21.],
                [3., 3.],
                [4., 3.],
            ]);
            path(&[[7., 3.], [7., 9.], [16., 9.], [16., 3.]]);
            path(&[[7., 21.], [7., 14.], [17., 14.], [17., 21.]]);
        }
        Icon::Undo | Icon::Redo => {
            let flip = |x| {
                if matches!(icon, Icon::Redo) {
                    24. - x
                } else {
                    x
                }
            };
            let points = [[8., 5.], [3., 10.], [8., 15.]];
            path(&points.map(|[x, y]| [flip(x), y]));
            path(&[
                [flip(3.), 10.],
                [flip(15.), 10.],
                [flip(20.), 13.],
                [flip(20.), 18.],
                [flip(17.), 21.],
            ]);
        }
        Icon::Layers => {
            path(&[[2., 8.], [12., 3.], [22., 8.], [12., 13.], [2., 8.]]);
            path(&[[2., 12.], [12., 17.], [22., 12.]]);
            path(&[[2., 16.], [12., 21.], [22., 16.]]);
        }
        Icon::Cube => {
            path(&[
                [3., 7.],
                [12., 2.],
                [21., 7.],
                [21., 17.],
                [12., 22.],
                [3., 17.],
                [3., 7.],
                [12., 12.],
                [21., 7.],
            ]);
            path(&[[12., 12.], [12., 22.]]);
        }
        Icon::Grid => {
            for x in [3., 10., 17.] {
                for y in [3., 10., 17.] {
                    painter.rect_stroke(
                        Rect::from_min_max(point(x, y), point(x + 4., y + 4.)),
                        0.0,
                        stroke,
                    );
                }
            }
        }
        Icon::Eye => {
            path(&[
                [2., 12.],
                [7., 7.],
                [12., 5.],
                [17., 7.],
                [22., 12.],
                [17., 17.],
                [12., 19.],
                [7., 17.],
                [2., 12.],
            ]);
            painter.circle_stroke(point(12., 12.), rect.width() / 7., stroke);
        }
        Icon::Sun => {
            painter.circle_stroke(point(12., 12.), rect.width() / 6., stroke);
            for i in 0..8 {
                let angle = i as f32 * std::f32::consts::TAU / 8.;
                let direction = egui::vec2(angle.cos(), angle.sin());
                painter.line_segment(
                    [
                        rect.center() + direction * rect.width() * 0.3,
                        rect.center() + direction * rect.width() * 0.43,
                    ],
                    stroke,
                );
            }
        }
        Icon::Compass => {
            path(&[[12., 2.], [22., 12.], [12., 22.], [2., 12.], [12., 2.]]);
            path(&[[12., 2.], [12., 22.]]);
            path(&[[2., 12.], [22., 12.]]);
        }
    }
}

/// Retains egui's keyboard interaction, focus ring and button semantics.
pub(super) fn icon_button(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let mut text = egui::text::LayoutJob::default();
    text.append(
        if label.is_empty() { " " } else { label },
        24.0,
        egui::TextFormat {
            font_id: egui::TextStyle::Button.resolve(ui.style()),
            color: Color32::PLACEHOLDER,
            ..Default::default()
        },
    );
    let response = ui.add(egui::Button::new(text).selected(selected));
    let accessible_label = if label.is_empty() {
        match icon {
            Icon::Undo => "Desfazer",
            Icon::Redo => "Refazer",
            _ => "Ação",
        }
    } else {
        label
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, accessible_label));
    let color = ui
        .style()
        .interact_selectable(&response, selected)
        .fg_stroke
        .color;
    let rect = Rect::from_center_size(
        Pos2::new(response.rect.left() + 17.0, response.rect.center().y),
        Vec2::splat(17.0),
    );
    paint_icon(ui.painter(), rect, icon, color);
    response
}

pub(super) fn toggle(ui: &mut Ui, icon: Icon, label: &str, value: &mut bool) -> Response {
    let mut response = icon_button(ui, icon, label, *value);
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, *value, label));
    response
}

pub(super) fn primary_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    ui.scope(|ui| {
        ui.visuals_mut().selection.bg_fill = Color32::from_rgb(27, 105, 194);
        ui.visuals_mut().selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(86, 165, 255));
        ui.visuals_mut().widgets.active.fg_stroke.color = Color32::WHITE;
        icon_button(ui, icon, label, true)
    })
    .inner
}

/// The same NSWE bit mapping as the document and world overlay. Open arrows
/// are outlined; blocked directions have a crossbar as well as a muted color.
pub(super) fn paint_nswe(
    painter: &egui::Painter,
    rect: Rect,
    mask: u8,
    open: Color32,
    closed: Color32,
) {
    let center = rect.center();
    let unit = rect.width().min(rect.height()) / 2.0;
    for (direction, axis) in [
        (Direction::North, egui::vec2(0., -1.)),
        (Direction::South, egui::vec2(0., 1.)),
        (Direction::West, egui::vec2(-1., 0.)),
        (Direction::East, egui::vec2(1., 0.)),
    ] {
        let allowed = mask & direction.bit() != 0;
        let color = if allowed { open } else { closed };
        let stroke = Stroke::new(1.5_f32, color);
        let normal = egui::vec2(-axis.y, axis.x);
        let tip = center + axis * unit * 0.84;
        let base = center + axis * unit * 0.42;
        painter.line_segment([base - normal * unit * 0.22, tip], stroke);
        painter.line_segment([base + normal * unit * 0.22, tip], stroke);
        if allowed {
            painter.line_segment([center + axis * unit * 0.22, tip], stroke);
        } else {
            painter.line_segment(
                [base - normal * unit * 0.22, base + normal * unit * 0.22],
                stroke,
            );
        }
    }
    painter.rect_stroke(
        Rect::from_center_size(center, Vec2::splat(unit * 0.25)),
        0.0,
        Stroke::new(1.0_f32, open),
    );
}

pub(super) fn empty_viewport(ui: &mut Ui, theme: EditorTheme) -> bool {
    let rect = ui.max_rect();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, background(theme));
    let grid = ui
        .visuals()
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.35);
    let horizon = rect.top() + rect.height() * 0.35;
    for i in -14..=14 {
        painter.line_segment(
            [
                egui::pos2(rect.center().x + i as f32 * 18., horizon),
                egui::pos2(rect.center().x + i as f32 * 110., rect.bottom()),
            ],
            Stroke::new(1.0_f32, grid),
        );
    }
    for i in 1..14 {
        let y = horizon + (i as f32 / 14.).powf(2.) * (rect.bottom() - horizon);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0_f32, grid),
        );
    }
    let center = rect.center() - egui::vec2(0., 45.);
    paint_icon(
        &painter,
        Rect::from_center_size(center - egui::vec2(0., 76.), Vec2::splat(92.)),
        Icon::Layers,
        accent(theme).gamma_multiply(0.8),
    );
    let content = Rect::from_center_size(
        center + egui::vec2(0., 55.),
        egui::vec2(rect.width().min(480.), 158.),
    );
    let mut clicked = false;
    ui.allocate_ui_at_rect(content, |ui| {
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("Seu próximo mundo começa aqui").size(23.0));
            ui.add_space(8.);
            ui.label(
                egui::RichText::new("Abra um projeto para editar a geodata do Lineage II.").weak(),
            );
            ui.label(
                egui::RichText::new("Selecione o cliente e o arquivo de geodata para começar.")
                    .weak()
                    .small(),
            );
            ui.add_space(19.);
            clicked = primary_button(ui, Icon::Folder, "Abrir projeto").clicked();
        });
    });
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 25.),
        egui::Align2::CENTER_CENTER,
        "L2J GEODATA  /  LINEAGE II",
        egui::FontId::monospace(11.),
        ui.visuals().weak_text_color(),
    );
    clicked
}
