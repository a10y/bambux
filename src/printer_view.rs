use std::sync::Arc;

use gpui::prelude::*;
use gpui::{Bounds, Hsla, IntoElement, PathBuilder, Pixels, Point, canvas, point, px};

use crate::{model::PrinterStatus, model_3mf::ModelMesh, theme::Theme};

#[derive(Debug)]
pub struct PrinterOrbit {
    pub yaw: f32,
    pub pitch: f32,
    pub dragging: Option<Point<Pixels>>,
}

impl Default for PrinterOrbit {
    fn default() -> Self {
        Self {
            yaw: -0.58,
            pitch: 0.30,
            dragging: None,
        }
    }
}

#[derive(Clone, Copy)]
struct V3 {
    x: f32,
    y: f32,
    z: f32,
}

impl V3 {
    const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

pub fn scene(
    status: &PrinterStatus,
    model: Option<Arc<ModelMesh>>,
    yaw: f32,
    pitch: f32,
    theme: Theme,
) -> impl IntoElement {
    let progress = status.progress;
    let layer_ratio = match (status.current_layer, status.total_layers) {
        (Some(current), Some(total)) if total > 0 => current as f32 / total as f32,
        _ => progress / 100.0,
    }
    .clamp(0.0, 1.0);
    let active = status.state == "RUNNING";

    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(gpui::fill(bounds, crate::theme::color(0x070708)));
            let scene = Scene::new(bounds, yaw, pitch);
            paint_floor(window, &scene, theme);
            paint_printer(window, &scene, layer_ratio, active, model.as_deref(), theme);
        },
    )
    .size_full()
}

struct Scene {
    center: Point<Pixels>,
    yaw: f32,
    pitch: f32,
    scale: f32,
}

impl Scene {
    fn new(bounds: Bounds<Pixels>, yaw: f32, pitch: f32) -> Self {
        Self {
            center: point(
                bounds.center().x,
                bounds.center().y + bounds.size.height * 0.06,
            ),
            yaw,
            pitch,
            scale: f32::from(bounds.size.width.min(bounds.size.height)) * 0.35,
        }
    }

    fn project(&self, p: V3) -> Point<Pixels> {
        let cy = self.yaw.cos();
        let sy = self.yaw.sin();
        let x = p.x * cy - p.z * sy;
        let depth = p.x * sy + p.z * cy;
        let cp = self.pitch.cos();
        let sp = self.pitch.sin();
        let y = p.y * cp - depth * sp;
        let depth = p.y * sp + depth * cp;
        let perspective = 1.0 / (1.0 + depth * 0.16);
        point(
            self.center.x + px(x * self.scale * perspective),
            self.center.y - px(y * self.scale * perspective),
        )
    }
}

fn paint_floor(window: &mut gpui::Window, scene: &Scene, theme: Theme) {
    for i in -6..=6 {
        let n = i as f32 * 0.18;
        line(
            window,
            scene.project(V3::new(-1.1, -1.0, n)),
            scene.project(V3::new(1.1, -1.0, n)),
            0.7,
            theme.accent.opacity(0.11),
        );
        line(
            window,
            scene.project(V3::new(n, -1.0, -1.1)),
            scene.project(V3::new(n, -1.0, 1.1)),
            0.7,
            theme.accent.opacity(0.11),
        );
    }
}

fn paint_printer(
    window: &mut gpui::Window,
    scene: &Scene,
    layer_ratio: f32,
    active: bool,
    model: Option<&ModelMesh>,
    theme: Theme,
) {
    let dim = theme.text_muted.opacity(0.30);
    let edge = theme.text_muted.opacity(0.82);
    let lit = theme.accent.opacity(0.90);

    polygon(
        window,
        scene,
        &[
            V3::new(-0.77, -0.92, -0.64),
            V3::new(0.77, -0.92, -0.64),
            V3::new(0.77, 0.96, -0.64),
            V3::new(-0.77, 0.96, -0.64),
        ],
        theme.accent.opacity(0.055),
        dim,
    );
    polygon(
        window,
        scene,
        &[
            V3::new(0.77, -0.92, -0.64),
            V3::new(0.77, -0.92, 0.64),
            V3::new(0.77, 0.96, 0.64),
            V3::new(0.77, 0.96, -0.64),
        ],
        theme.accent.opacity(0.035),
        dim,
    );

    let corners = [
        V3::new(-0.77, -0.92, -0.64),
        V3::new(0.77, -0.92, -0.64),
        V3::new(0.77, -0.92, 0.64),
        V3::new(-0.77, -0.92, 0.64),
        V3::new(-0.77, 0.96, -0.64),
        V3::new(0.77, 0.96, -0.64),
        V3::new(0.77, 0.96, 0.64),
        V3::new(-0.77, 0.96, 0.64),
    ];
    for (a, b) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ] {
        line(
            window,
            scene.project(corners[a]),
            scene.project(corners[b]),
            1.65,
            edge,
        );
    }

    polygon(
        window,
        scene,
        &[
            V3::new(-0.82, -0.98, -0.69),
            V3::new(0.82, -0.98, -0.69),
            V3::new(0.82, -0.98, 0.69),
            V3::new(-0.82, -0.98, 0.69),
        ],
        crate::theme::color(0x28282d).opacity(0.92),
        theme.border_strong,
    );
    line(
        window,
        scene.project(V3::new(-0.70, 0.62, -0.52)),
        scene.project(V3::new(0.70, 0.62, -0.52)),
        1.3,
        lit,
    );
    line(
        window,
        scene.project(V3::new(-0.70, 0.62, 0.52)),
        scene.project(V3::new(0.70, 0.62, 0.52)),
        1.0,
        dim,
    );

    let bed_y = -0.72 + layer_ratio * 0.14;
    polygon(
        window,
        scene,
        &[
            V3::new(-0.58, bed_y, -0.48),
            V3::new(0.58, bed_y, -0.48),
            V3::new(0.58, bed_y, 0.48),
            V3::new(-0.58, bed_y, 0.48),
        ],
        crate::theme::color(0x484558).opacity(0.72),
        theme.text_muted.opacity(0.58),
    );

    let base = bed_y + 0.02;
    let top = if let Some(mesh) = model {
        paint_model(window, scene, mesh, base, layer_ratio, theme)
    } else {
        let top = base + 0.08 + layer_ratio * 0.82;
        polygon(
            window,
            scene,
            &[
                V3::new(-0.28, base, -0.22),
                V3::new(0.30, base, -0.19),
                V3::new(0.24, top, -0.13),
                V3::new(-0.20, top, -0.16),
            ],
            theme.accent.opacity(0.28),
            lit,
        );
        polygon(
            window,
            scene,
            &[
                V3::new(0.30, base, -0.19),
                V3::new(0.27, base, 0.20),
                V3::new(0.20, top, 0.13),
                V3::new(0.24, top, -0.13),
            ],
            theme.accent_strong.opacity(0.28),
            theme.accent,
        );
        polygon(
            window,
            scene,
            &[
                V3::new(-0.20, top, -0.16),
                V3::new(0.24, top, -0.13),
                V3::new(0.20, top, 0.13),
                V3::new(-0.17, top, 0.15),
            ],
            theme.accent.opacity(0.42),
            lit,
        );
        top
    };

    let head_y = (top + 0.11).min(0.58);
    polygon(
        window,
        scene,
        &[
            V3::new(-0.15, head_y + 0.13, -0.17),
            V3::new(0.15, head_y + 0.13, -0.17),
            V3::new(0.15, head_y - 0.08, -0.17),
            V3::new(-0.15, head_y - 0.08, -0.17),
        ],
        crate::theme::color(0x25252a),
        theme.text_muted,
    );
    line(
        window,
        scene.project(V3::new(0.0, head_y - 0.08, -0.12)),
        scene.project(V3::new(0.0, top + 0.01, -0.10)),
        1.4,
        theme.warning,
    );

    let beacon = scene.project(V3::new(-0.67, 0.82, 0.66));
    circle(
        window,
        beacon,
        4.0,
        if active { theme.success } else { theme.accent },
    );
}

fn line(window: &mut gpui::Window, a: Point<Pixels>, b: Point<Pixels>, width: f32, color: Hsla) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(a);
    path.line_to(b);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn polygon(window: &mut gpui::Window, scene: &Scene, vertices: &[V3], fill: Hsla, stroke: Hsla) {
    if vertices.is_empty() {
        return;
    }
    let points: Vec<_> = vertices.iter().map(|p| scene.project(*p)).collect();
    let mut path = PathBuilder::fill();
    path.move_to(points[0]);
    for point in points.iter().skip(1) {
        path.line_to(*point);
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, fill);
    }
    for pair in points.windows(2) {
        line(window, pair[0], pair[1], 1.0, stroke);
    }
    line(window, *points.last().unwrap(), points[0], 1.0, stroke);
}

fn circle(window: &mut gpui::Window, center: Point<Pixels>, radius: f32, color: Hsla) {
    let mut path = PathBuilder::fill();
    for i in 0..=20 {
        let angle = std::f32::consts::TAU * i as f32 / 20.0;
        let p = point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        );
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// Paint the parsed 3MF mesh onto the bed, clipped at the current layer
/// height. Returns the world-space Y of the clip top so the print head
/// hovers just above the exposed surface.
fn paint_model(
    window: &mut gpui::Window,
    scene: &Scene,
    mesh: &ModelMesh,
    base_y: f32,
    layer_ratio: f32,
    theme: Theme,
) -> f32 {
    let [min_x, min_y, min_z] = mesh.min;
    let [max_x, max_y, max_z] = mesh.max;
    let span_x = (max_x - min_x).max(1e-3);
    let span_y = (max_y - min_y).max(1e-3);
    let span_z = (max_z - min_z).max(1e-3);
    // Fit the largest bed-plane dimension into the enclosure (~1.0 world units).
    let scale = 0.95 / span_x.max(span_y);
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    let height = span_z * scale;
    let clip_mm = (span_z * layer_ratio).max(0.01);
    let clip_z = height * layer_ratio;

    let to_world = |p: [f32; 3]| V3 {
        x: (p[0] - cx) * scale,
        y: base_y + p[2] * scale,
        z: (p[1] - cy) * scale,
    };

    let fill = theme.accent.opacity(0.30);
    let stroke = theme.accent.opacity(0.65);
    let mut fill_path = PathBuilder::fill();
    let mut stroke_path = PathBuilder::stroke(px(0.6));
    let mut any_fill = false;
    let mut any_stroke = false;

    for tri in &mesh.triangles {
        if tri[0][2] > clip_mm || tri[1][2] > clip_mm || tri[2][2] > clip_mm {
            continue;
        }
        let a = scene.project(to_world(tri[0]));
        let b = scene.project(to_world(tri[1]));
        let c = scene.project(to_world(tri[2]));
        // Backface cull using signed screen-space area.
        let area = (f32::from(b.x) - f32::from(a.x)) * (f32::from(c.y) - f32::from(a.y))
            - (f32::from(c.x) - f32::from(a.x)) * (f32::from(b.y) - f32::from(a.y));
        if area <= 0.0 {
            continue;
        }
        fill_path.move_to(a);
        fill_path.line_to(b);
        fill_path.line_to(c);
        fill_path.close();
        any_fill = true;
        stroke_path.move_to(a);
        stroke_path.line_to(b);
        stroke_path.line_to(c);
        stroke_path.close();
        any_stroke = true;
    }

    if any_fill && let Ok(path) = fill_path.build() {
        window.paint_path(path, fill);
    }
    if any_stroke && let Ok(path) = stroke_path.build() {
        window.paint_path(path, stroke);
    }

    base_y + clip_z
}
