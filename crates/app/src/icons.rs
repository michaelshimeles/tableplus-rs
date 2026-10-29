//! Small vector icons drawn with the painter (crisp at any scale, no image assets).

use std::f32::consts::{PI, TAU};

use eframe::egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, pos2, vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Table,
    View,
    MatView,
    Function,
    Procedure,
    Database,
    Schema,
    Search,
    Plus,
    Minus,
    Close,
    ChevronRight,
    ChevronDown,
    ChevronLeft,
    Play,
    PlayAll,
    Stop,
    Refresh,
    Filter,
    Sun,
    Moon,
    Key,
    Sql,
    Export,
    Home,
    Trash,
    Lock,
    Copy,
    Plan,
    Stopwatch,
}

/// Maps a 16×16 design grid onto `rect`.
struct Grid {
    origin: Pos2,
    scale: f32,
}

impl Grid {
    fn new(rect: Rect) -> Self {
        let side = rect.width().min(rect.height());
        let origin = pos2(rect.center().x - side / 2.0, rect.center().y - side / 2.0);
        Self { origin, scale: side / 16.0 }
    }
    fn p(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.origin.x + x * self.scale, self.origin.y + y * self.scale)
    }
    fn r(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(self.p(x0, y0), self.p(x1, y1))
    }
    fn len(&self, v: f32) -> f32 {
        v * self.scale
    }
}

fn arc(g: &Grid, cx: f32, cy: f32, r: f32, from: f32, to: f32) -> Vec<Pos2> {
    let n = 18;
    (0..=n)
        .map(|i| {
            let t = from + (to - from) * i as f32 / n as f32;
            g.p(cx + r * t.cos(), cy + r * t.sin())
        })
        .collect()
}

pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let g = Grid::new(rect);
    let w = (g.len(1.3)).max(1.0);
    let s = Stroke::new(w, color);
    let line = |pts: Vec<Pos2>| {
        painter.add(Shape::line(pts, s));
    };
    match icon {
        Icon::Table => {
            let r = g.r(2.0, 3.0, 14.0, 13.0);
            painter.rect_stroke(r, CornerRadius::same(2), s, StrokeKind::Middle);
            painter.rect_filled(
                g.r(2.0, 3.0, 14.0, 6.0),
                CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 },
                color.gamma_multiply(0.55),
            );
            painter.line_segment([g.p(2.0, 9.5), g.p(14.0, 9.5)], s);
            painter.line_segment([g.p(7.0, 6.0), g.p(7.0, 13.0)], s);
        }
        Icon::View => {
            let r = g.r(2.0, 3.0, 14.0, 13.0);
            painter.rect_stroke(r, CornerRadius::same(2), s, StrokeKind::Middle);
            // eye
            let mut pts = arc(&g, 8.0, 11.2, 4.6, -PI * 0.78, -PI * 0.22);
            pts.extend(arc(&g, 8.0, 4.8, 4.6, PI * 0.22, PI * 0.78));
            pts.push(pts[0]);
            line(pts);
            painter.circle_filled(g.p(8.0, 8.0), g.len(1.4), color);
        }
        Icon::MatView => {
            let r = g.r(2.0, 3.0, 14.0, 13.0);
            painter.rect_stroke(r, CornerRadius::same(2), s, StrokeKind::Middle);
            painter.circle_filled(g.p(8.0, 8.0), g.len(2.2), color);
        }
        Icon::Function | Icon::Procedure => {
            let font = eframe::egui::FontId::new(g.len(12.5), crate::theme::semibold());
            let txt = if icon == Icon::Function { "ƒ" } else { "P" };
            painter.text(g.p(8.0, 8.4), eframe::egui::Align2::CENTER_CENTER, txt, font, color);
        }
        Icon::Database => {
            let top = g.r(3.0, 2.0, 13.0, 5.5);
            painter.add(Shape::ellipse_stroke(top.center(), top.size() / 2.0, s));
            line(vec![g.p(3.0, 3.75), g.p(3.0, 12.25)]);
            line(vec![g.p(13.0, 3.75), g.p(13.0, 12.25)]);
            line(
                arc(&g, 8.0, 12.25, 5.0, 0.0, PI)
                    .into_iter()
                    .map(|p| pos2(p.x, g.origin.y + (p.y - g.origin.y - g.len(12.25)) * 0.35 + g.len(12.25)))
                    .collect(),
            );
            line(
                arc(&g, 8.0, 8.0, 5.0, 0.0, PI)
                    .into_iter()
                    .map(|p| pos2(p.x, g.origin.y + (p.y - g.origin.y - g.len(8.0)) * 0.35 + g.len(8.0)))
                    .collect(),
            );
        }
        Icon::Schema => {
            let pts = vec![
                g.p(2.0, 4.0),
                g.p(6.0, 4.0),
                g.p(7.5, 5.5),
                g.p(14.0, 5.5),
                g.p(14.0, 13.0),
                g.p(2.0, 13.0),
                g.p(2.0, 4.0),
            ];
            line(pts);
        }
        Icon::Search => {
            painter.circle_stroke(g.p(7.0, 7.0), g.len(4.2), s);
            painter.line_segment([g.p(10.2, 10.2), g.p(13.8, 13.8)], Stroke::new(w * 1.3, color));
        }
        Icon::Plus => {
            painter.line_segment([g.p(8.0, 3.0), g.p(8.0, 13.0)], s);
            painter.line_segment([g.p(3.0, 8.0), g.p(13.0, 8.0)], s);
        }
        Icon::Minus => {
            painter.line_segment([g.p(3.0, 8.0), g.p(13.0, 8.0)], s);
        }
        Icon::Close => {
            painter.line_segment([g.p(4.5, 4.5), g.p(11.5, 11.5)], s);
            painter.line_segment([g.p(11.5, 4.5), g.p(4.5, 11.5)], s);
        }
        Icon::ChevronRight => line(vec![g.p(6.0, 3.5), g.p(10.5, 8.0), g.p(6.0, 12.5)]),
        Icon::ChevronLeft => line(vec![g.p(10.0, 3.5), g.p(5.5, 8.0), g.p(10.0, 12.5)]),
        Icon::ChevronDown => line(vec![g.p(3.5, 6.0), g.p(8.0, 10.5), g.p(12.5, 6.0)]),
        Icon::Play => {
            painter.add(Shape::convex_polygon(
                vec![g.p(4.5, 2.5), g.p(13.0, 8.0), g.p(4.5, 13.5)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::PlayAll => {
            painter.add(Shape::convex_polygon(vec![g.p(2.0, 3.0), g.p(8.5, 8.0), g.p(2.0, 13.0)], color, Stroke::NONE));
            painter.add(Shape::convex_polygon(
                vec![g.p(8.0, 3.0), g.p(14.5, 8.0), g.p(8.0, 13.0)],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Stop => {
            painter.rect_filled(g.r(3.5, 3.5, 12.5, 12.5), CornerRadius::same(2), color);
        }
        Icon::Refresh => {
            line(arc(&g, 8.0, 8.0, 5.0, -PI * 0.35, PI * 1.45));
            let tip = g.p(8.0 + 5.0 * (-PI * 0.35_f32).cos(), 8.0 + 5.0 * (-PI * 0.35_f32).sin());
            painter.add(Shape::convex_polygon(
                vec![
                    tip + vec2(g.len(-2.6), g.len(-0.6)),
                    tip + vec2(g.len(1.6), g.len(-2.4)),
                    tip + vec2(g.len(1.0), g.len(2.2)),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Filter => {
            line(vec![
                g.p(2.5, 3.5),
                g.p(13.5, 3.5),
                g.p(9.0, 8.5),
                g.p(9.0, 13.0),
                g.p(7.0, 12.0),
                g.p(7.0, 8.5),
                g.p(2.5, 3.5),
            ]);
        }
        Icon::Sun => {
            painter.circle_filled(g.p(8.0, 8.0), g.len(3.0), color);
            for i in 0..8 {
                let a = i as f32 * TAU / 8.0;
                painter.line_segment(
                    [g.p(8.0 + 5.0 * a.cos(), 8.0 + 5.0 * a.sin()), g.p(8.0 + 6.8 * a.cos(), 8.0 + 6.8 * a.sin())],
                    s,
                );
            }
        }
        Icon::Moon => {
            // Half-filled circle: the usual "appearance" glyph.
            painter.circle_stroke(g.p(8.0, 8.0), g.len(5.5), s);
            let mut half = arc(&g, 8.0, 8.0, 5.5, PI * 0.5, PI * 1.5);
            half.push(g.p(8.0, 2.5));
            painter.add(Shape::convex_polygon(half, color, Stroke::NONE));
        }
        Icon::Key => {
            painter.circle_stroke(g.p(5.5, 8.0), g.len(3.0), s);
            line(vec![g.p(8.5, 8.0), g.p(14.0, 8.0), g.p(14.0, 10.5)]);
            line(vec![g.p(12.0, 8.0), g.p(12.0, 10.0)]);
        }
        Icon::Sql => {
            painter.rect_stroke(g.r(1.5, 3.0, 14.5, 13.0), CornerRadius::same(2), s, StrokeKind::Middle);
            line(vec![g.p(4.0, 6.0), g.p(6.0, 8.0), g.p(4.0, 10.0)]);
            line(vec![g.p(7.5, 10.5), g.p(11.5, 10.5)]);
        }
        Icon::Export => {
            line(vec![g.p(8.0, 10.0), g.p(8.0, 2.5)]);
            line(vec![g.p(5.0, 5.5), g.p(8.0, 2.5), g.p(11.0, 5.5)]);
            line(vec![g.p(3.0, 9.0), g.p(3.0, 13.5), g.p(13.0, 13.5), g.p(13.0, 9.0)]);
        }
        Icon::Home => {
            line(vec![g.p(2.0, 8.0), g.p(8.0, 2.5), g.p(14.0, 8.0)]);
            line(vec![g.p(4.0, 6.5), g.p(4.0, 13.5), g.p(12.0, 13.5), g.p(12.0, 6.5)]);
        }
        Icon::Trash => {
            line(vec![g.p(2.5, 4.0), g.p(13.5, 4.0)]);
            line(vec![g.p(6.0, 4.0), g.p(6.5, 2.5), g.p(9.5, 2.5), g.p(10.0, 4.0)]);
            line(vec![g.p(4.0, 4.0), g.p(4.8, 13.5), g.p(11.2, 13.5), g.p(12.0, 4.0)]);
        }
        Icon::Lock => {
            painter.rect_stroke(g.r(3.5, 7.0, 12.5, 13.5), CornerRadius::same(2), s, StrokeKind::Middle);
            line(arc(&g, 8.0, 7.0, 3.0, PI, TAU));
        }
        Icon::Copy => {
            painter.rect_stroke(g.r(5.0, 5.0, 13.5, 13.5), CornerRadius::same(2), s, StrokeKind::Middle);
            line(vec![g.p(2.5, 10.5), g.p(2.5, 2.5), g.p(10.5, 2.5)]);
        }
        Icon::Plan => {
            // A step with two steps below it.
            painter.rect_filled(g.r(1.5, 2.0, 8.5, 5.5), CornerRadius::same(1), color);
            line(vec![g.p(4.0, 5.5), g.p(4.0, 12.5), g.p(7.0, 12.5)]);
            line(vec![g.p(4.0, 8.5), g.p(7.0, 8.5)]);
            painter.rect_stroke(g.r(7.5, 7.0, 14.5, 10.0), CornerRadius::same(1), s, StrokeKind::Middle);
            painter.rect_stroke(g.r(7.5, 11.0, 14.5, 14.0), CornerRadius::same(1), s, StrokeKind::Middle);
        }
        Icon::Stopwatch => {
            painter.circle_stroke(g.p(8.0, 9.0), g.len(5.2), s);
            line(vec![g.p(6.5, 1.8), g.p(9.5, 1.8)]);
            line(vec![g.p(8.0, 1.8), g.p(8.0, 3.8)]);
            line(vec![g.p(8.0, 9.0), g.p(10.4, 6.6)]);
            painter.circle_filled(g.p(8.0, 9.0), g.len(0.9), color);
        }
    }
}

/// Color used for object kinds in the sidebar and quick open.
pub fn kind_color(kind: tableplus_core::model::ObjectKind, dark: bool) -> Color32 {
    use tableplus_core::model::ObjectKind::*;
    match (kind, dark) {
        (Table, false) => Color32::from_rgb(0x3D, 0x7E, 0xD8),
        (Table, true) => Color32::from_rgb(0x6A, 0xA6, 0xF8),
        (View | MaterializedView, false) => Color32::from_rgb(0x8E, 0x4E, 0xC6),
        (View | MaterializedView, true) => Color32::from_rgb(0xB7, 0x8C, 0xF0),
        (Function | Procedure, false) => Color32::from_rgb(0x2E, 0x8B, 0x57),
        (Function | Procedure, true) => Color32::from_rgb(0x5F, 0xC9, 0x8C),
    }
}

pub fn kind_icon(kind: tableplus_core::model::ObjectKind) -> Icon {
    use tableplus_core::model::ObjectKind::*;
    match kind {
        Table => Icon::Table,
        View => Icon::View,
        MaterializedView => Icon::MatView,
        Function => Icon::Function,
        Procedure => Icon::Procedure,
    }
}
