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
    /// Foreshortening per world unit of depth; 0 is orthographic.
    perspective: f32,
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
            perspective: 0.16,
        }
    }

    /// Orthographic camera centred on the bounds, for inspecting a model
    /// without keystone distortion. One world unit is half the short side.
    fn orthographic(bounds: Bounds<Pixels>, yaw: f32, pitch: f32) -> Self {
        Self {
            center: bounds.center(),
            yaw,
            pitch,
            scale: f32::from(bounds.size.width.min(bounds.size.height)) * 0.5,
            perspective: 0.0,
        }
    }

    /// Rotate a world-space vector into camera space, where `z` is depth
    /// away from the viewer.
    fn camera(&self, p: V3) -> V3 {
        let cy = self.yaw.cos();
        let sy = self.yaw.sin();
        let x = p.x * cy - p.z * sy;
        let depth = p.x * sy + p.z * cy;
        let cp = self.pitch.cos();
        let sp = self.pitch.sin();
        V3::new(x, p.y * cp - depth * sp, p.y * sp + depth * cp)
    }

    fn project(&self, p: V3) -> Point<Pixels> {
        let c = self.camera(p);
        let perspective = 1.0 / (1.0 + c.z * self.perspective);
        point(
            self.center.x + px(c.x * self.scale * perspective),
            self.center.y - px(c.y * self.scale * perspective),
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
    let clip_mm = min_z + (span_z * layer_ratio).max(0.01);
    let clip_z = height * layer_ratio;

    let to_world = |p: [f32; 3]| V3 {
        x: (p[0] - cx) * scale,
        y: base_y + (p[2] - min_z) * scale,
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

    let mut toolpath = PathBuilder::stroke(px(0.7));
    let mut any_toolpath = false;
    for segment in &mesh.toolpaths {
        if segment[0][2].max(segment[1][2]) > clip_mm {
            continue;
        }
        toolpath.move_to(scene.project(to_world(segment[0])));
        toolpath.line_to(scene.project(to_world(segment[1])));
        any_toolpath = true;
    }
    if any_toolpath && let Ok(path) = toolpath.build() {
        window.paint_path(path, theme.accent.opacity(0.6));
    }

    base_y + clip_z
}

/// Standalone 3D preview of a model file: shaded, depth-sorted faces for
/// meshes, height-tinted extrusion paths for sliced G-code.
pub fn model_view(model: Arc<ModelMesh>, yaw: f32, pitch: f32, theme: Theme) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            window.paint_quad(gpui::fill(bounds, crate::theme::color(0x070708)));
            let scene = Scene::orthographic(bounds, yaw, pitch);
            paint_preview(window, &scene, &model, theme);
        },
    )
    .size_full()
}

/// World units the model's bounding-box diagonal is fitted to. An
/// orthographic view of a box never spans more than its diagonal, so the
/// part stays in frame at every orbit angle.
const PREVIEW_DIAGONAL: f32 = 1.85;
const SHADE_LEVELS: f32 = 24.0;

fn paint_preview(window: &mut gpui::Window, scene: &Scene, mesh: &ModelMesh, theme: Theme) {
    let [min_x, min_y, min_z] = mesh.min;
    let [max_x, max_y, max_z] = mesh.max;
    let diagonal = ((max_x - min_x).powi(2) + (max_y - min_y).powi(2) + (max_z - min_z).powi(2))
        .sqrt()
        .max(1e-3);
    let scale = PREVIEW_DIAGONAL / diagonal;
    let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    // Centre the bounding box on the orbit pivot, so the part sits mid-frame.
    let floor = -(max_z - min_z) * scale / 2.0;
    // Printer space is Z-up and right-handed; the scene is Y-up. Map
    // (x, y, z) -> (x, z, -y), a proper rotation, so face winding survives.
    let to_world = |p: [f32; 3]| V3 {
        x: (p[0] - cx) * scale,
        y: floor + (p[2] - min_z) * scale,
        z: -(p[1] - cy) * scale,
    };

    // Build plate under the part's footprint.
    let half_x = (max_x - min_x) * scale / 2.0 + 0.12;
    let half_z = (max_y - min_y) * scale / 2.0 + 0.12;
    let plate_y = floor - 0.01;
    polygon(
        window,
        scene,
        &[
            V3::new(-half_x, plate_y, -half_z),
            V3::new(half_x, plate_y, -half_z),
            V3::new(half_x, plate_y, half_z),
            V3::new(-half_x, plate_y, half_z),
        ],
        crate::theme::color(0x484558).opacity(0.45),
        theme.text_muted.opacity(0.35),
    );
    let steps = 8;
    for i in 1..steps {
        let t = i as f32 / steps as f32;
        let x = -half_x + 2.0 * half_x * t;
        let z = -half_z + 2.0 * half_z * t;
        let grid = theme.text_muted.opacity(0.10);
        line(
            window,
            scene.project(V3::new(x, plate_y, -half_z)),
            scene.project(V3::new(x, plate_y, half_z)),
            0.6,
            grid,
        );
        line(
            window,
            scene.project(V3::new(-half_x, plate_y, z)),
            scene.project(V3::new(half_x, plate_y, z)),
            0.6,
            grid,
        );
    }

    if !mesh.triangles.is_empty() {
        paint_shaded(window, scene, &mesh.triangles, &to_world, theme);
    }
    if !mesh.toolpaths.is_empty() {
        paint_toolpaths(window, scene, mesh, &to_world, theme);
    }
}

fn paint_shaded(
    window: &mut gpui::Window,
    scene: &Scene,
    triangles: &[crate::model_3mf::Triangle],
    to_world: &dyn Fn([f32; 3]) -> V3,
    theme: Theme,
) {
    // Key light from upper-left, slightly toward the viewer (camera space).
    let light = normalize(V3::new(-0.45, 0.75, -0.5));
    let mut faces: Vec<(f32, u8, [Point<Pixels>; 3])> = triangles
        .iter()
        .filter_map(|tri| {
            let [a, b, c] = tri.map(|p| scene.camera(to_world(p)));
            let normal = normalize(cross(sub(b, a), sub(c, a)));
            if normal.z >= 0.0 {
                return None; // facing away from the camera
            }
            let lambert = dot(normal, light).max(0.0);
            let shade = ((0.22 + 0.78 * lambert) * SHADE_LEVELS).round() as u8;
            let depth = (a.z + b.z + c.z) / 3.0;
            let points = tri.map(|p| scene.project(to_world(p)));
            Some((depth, shade, points))
        })
        .collect();
    // Painter's algorithm: far faces first.
    faces.sort_by(|a, b| b.0.total_cmp(&a.0));

    let base = theme.accent;
    let mut index = 0;
    while index < faces.len() {
        // Batch runs of equally shaded faces into one path to keep draw calls low.
        let shade = faces[index].1;
        let mut path = PathBuilder::fill();
        while index < faces.len() && faces[index].1 == shade {
            let [a, b, c] = faces[index].2;
            path.move_to(a);
            path.line_to(b);
            path.line_to(c);
            path.close();
            index += 1;
        }
        let t = shade as f32 / SHADE_LEVELS;
        let color = Hsla {
            l: (base.l * (0.25 + 0.95 * t)).min(0.92),
            s: base.s * (0.55 + 0.45 * t),
            a: 1.0,
            ..base
        };
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

fn paint_toolpaths(
    window: &mut gpui::Window,
    scene: &Scene,
    mesh: &ModelMesh,
    to_world: &dyn Fn([f32; 3]) -> V3,
    theme: Theme,
) {
    const BANDS: usize = 16;
    let [_, _, min_z] = mesh.min;
    let span_z = (mesh.max[2] - min_z).max(1e-3);
    let mut bands: Vec<PathBuilder> = (0..BANDS).map(|_| PathBuilder::stroke(px(0.8))).collect();
    for segment in &mesh.toolpaths {
        let height = (segment[0][2] - min_z) / span_z;
        let band = ((height * BANDS as f32) as usize).min(BANDS - 1);
        bands[band].move_to(scene.project(to_world(segment[0])));
        bands[band].line_to(scene.project(to_world(segment[1])));
    }
    // Tint from cool at the first layer to the accent at the top.
    for (index, band) in bands.into_iter().enumerate() {
        let t = index as f32 / (BANDS - 1) as f32;
        let color = Hsla {
            h: theme.cyan.h + (theme.accent.h - theme.cyan.h) * t,
            s: theme.cyan.s + (theme.accent.s - theme.cyan.s) * t,
            l: theme.cyan.l + (theme.accent.l - theme.cyan.l) * t,
            a: 0.85,
        };
        if let Ok(path) = band.build() {
            window.paint_path(path, color);
        }
    }
}

fn sub(a: V3, b: V3) -> V3 {
    V3::new(a.x - b.x, a.y - b.y, a.z - b.z)
}

fn cross(a: V3, b: V3) -> V3 {
    V3::new(
        a.y * b.z - a.z * b.y,
        a.z * b.x - a.x * b.z,
        a.x * b.y - a.y * b.x,
    )
}

fn dot(a: V3, b: V3) -> f32 {
    a.x * b.x + a.y * b.y + a.z * b.z
}

fn normalize(v: V3) -> V3 {
    let length = dot(v, v).sqrt().max(1e-9);
    V3::new(v.x / length, v.y / length, v.z / length)
}
