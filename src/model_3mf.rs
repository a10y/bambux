use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
};

use anyhow::{Context as _, Result, anyhow, bail};

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_MODEL_XML_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_TRIANGLES: usize = 2_000_000;
const MAX_RENDER_TRIANGLES: usize = 12_000;
const MAX_RECURSION: usize = 16;
/// Toolpath segments kept for drawing; whole layers are dropped to stay under this.
const MAX_RENDER_SEGMENTS: usize = 40_000;
/// Consecutive extrusion moves are merged until they span at least this many millimetres.
const MIN_SEGMENT_MM: f32 = 0.4;
/// Arcs are flattened into chords of roughly this length.
const ARC_CHORD_MM: f32 = 1.0;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelMesh {
    pub name: String,
    pub triangles: Vec<Triangle>,
    /// Extrusion paths from sliced G-code, used when the 3MF carries no mesh
    /// (Bambu `.gcode.3mf` exports strip the geometry).
    pub toolpaths: Vec<Segment>,
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub source_triangles: usize,
}

#[allow(dead_code)]
pub type Triangle = [[f32; 3]; 3];

pub type Segment = [[f32; 3]; 2];

#[allow(dead_code)]
type Transform = [[f32; 4]; 3];

#[derive(Clone)]
#[allow(dead_code)]
enum ObjectDef {
    Mesh {
        vertices: Vec<[f32; 3]>,
        triangles: Vec<[u32; 3]>,
    },
    Components(Vec<(u32, Transform)>),
}

#[allow(dead_code)]
pub fn parse_3mf(name: String, bytes: Vec<u8>) -> Result<ModelMesh> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "3MF is larger than the {} MiB safety limit",
            MAX_ARCHIVE_BYTES / 1024 / 1024
        );
    }
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("invalid 3MF ZIP container")?;
    let model_index = (0..archive.len())
        .find(|index| {
            archive
                .by_index(*index)
                .ok()
                .is_some_and(|entry| entry.name().to_ascii_lowercase().ends_with(".model"))
        })
        .ok_or_else(|| anyhow!("3MF contains no .model document"))?;
    let entry = archive.by_index(model_index)?;
    if entry.size() > MAX_MODEL_XML_BYTES {
        bail!("3MF model document exceeds the 64 MiB safety limit");
    }
    let mut xml = String::with_capacity(entry.size() as usize);
    entry
        .take(MAX_MODEL_XML_BYTES + 1)
        .read_to_string(&mut xml)
        .context("3MF model XML is not valid UTF-8")?;
    let triangles = mesh_triangles(&xml)?;
    if !triangles.is_empty() {
        return Ok(from_triangles(name, triangles));
    }

    // Sliced exports keep only G-code; preview the first plate's toolpaths instead.
    let gcode_index = (0..archive.len())
        .filter_map(|index| {
            let entry = archive.by_index(index).ok()?;
            let plate = entry
                .name()
                .strip_prefix("Metadata/plate_")?
                .strip_suffix(".gcode")?
                .parse::<u32>()
                .ok()?;
            Some((plate, index))
        })
        .min()
        .map(|(_, index)| index)
        .ok_or_else(|| anyhow!("3MF contains no mesh or sliced G-code"))?;
    let toolpaths = parse_gcode(BufReader::new(archive.by_index(gcode_index)?))?;
    if toolpaths.is_empty() {
        bail!("3MF G-code contains no printable extrusion");
    }
    let (min, max) = bounds(toolpaths.iter().flatten());
    Ok(ModelMesh {
        name,
        triangles: Vec::new(),
        toolpaths,
        min,
        max,
        source_triangles: 0,
    })
}

fn from_triangles(name: String, expanded: Vec<Triangle>) -> ModelMesh {
    let source_triangles = expanded.len();
    let triangles: Vec<Triangle> = if source_triangles > MAX_RENDER_TRIANGLES {
        let step = source_triangles as f64 / MAX_RENDER_TRIANGLES as f64;
        (0..MAX_RENDER_TRIANGLES)
            .map(|index| expanded[(index as f64 * step) as usize])
            .collect()
    } else {
        expanded
    };
    let (min, max) = bounds(triangles.iter().flatten());
    ModelMesh {
        name,
        triangles,
        toolpaths: Vec::new(),
        min,
        max,
        source_triangles,
    }
}

/// Expand the build items of a 3MF model document into world-space triangles.
/// Returns an empty list when the document has no geometry.
fn mesh_triangles(xml: &str) -> Result<Vec<Triangle>> {
    let document = roxmltree::Document::parse(xml).context("invalid 3MF model XML")?;
    let model = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "model")
        .ok_or_else(|| anyhow!("3MF model root is missing"))?;
    let unit = unit_scale(model.attribute("unit").unwrap_or("millimeter"));
    let mut objects = HashMap::new();

    for object in model
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "object")
    {
        let Some(id) = object
            .attribute("id")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(mesh) = object
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "mesh")
        {
            let mut vertices = Vec::new();
            if let Some(vertices_node) = mesh
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == "vertices")
            {
                for vertex in vertices_node
                    .children()
                    .filter(|node| node.is_element() && node.tag_name().name() == "vertex")
                {
                    if vertices.len() >= MAX_SOURCE_TRIANGLES * 3 {
                        bail!("3MF vertex count exceeds the safety limit");
                    }
                    vertices.push([
                        attr_f32(vertex, "x")? * unit,
                        attr_f32(vertex, "y")? * unit,
                        attr_f32(vertex, "z")? * unit,
                    ]);
                }
            }
            let mut triangles = Vec::new();
            if let Some(triangles_node) = mesh
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == "triangles")
            {
                for triangle in triangles_node
                    .children()
                    .filter(|node| node.is_element() && node.tag_name().name() == "triangle")
                {
                    if triangles.len() >= MAX_SOURCE_TRIANGLES {
                        bail!("3MF triangle count exceeds the safety limit");
                    }
                    triangles.push([
                        attr_u32(triangle, "v1")?,
                        attr_u32(triangle, "v2")?,
                        attr_u32(triangle, "v3")?,
                    ]);
                }
            }
            objects.insert(
                id,
                ObjectDef::Mesh {
                    vertices,
                    triangles,
                },
            );
        } else if let Some(components_node) = object
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == "components")
        {
            let components = components_node
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "component")
                .filter_map(|component| {
                    let object_id = component.attribute("objectid")?.parse::<u32>().ok()?;
                    Some((object_id, parse_transform(component.attribute("transform"))))
                })
                .collect();
            objects.insert(id, ObjectDef::Components(components));
        }
    }

    let build_items: Vec<_> = model
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "item")
        .filter_map(|item| {
            let id = item.attribute("objectid")?.parse::<u32>().ok()?;
            Some((id, parse_transform(item.attribute("transform"))))
        })
        .collect();

    let mut expanded = Vec::new();
    if build_items.is_empty() {
        for (&id, object) in &objects {
            if matches!(object, ObjectDef::Mesh { .. }) {
                expand_object(id, identity(), 0, &objects, &mut expanded)?;
            }
        }
    } else {
        for (id, transform) in build_items {
            expand_object(id, transform, 0, &objects, &mut expanded)?;
        }
    }
    Ok(expanded)
}

/// G-code features that outline the part. Infill, supports, and start/end
/// sequences (`Custom`) are skipped to keep the preview readable.
fn feature_is_shown(feature: &str) -> bool {
    let feature = feature.to_ascii_lowercase();
    !(feature.contains("infill")
        || feature.contains("support")
        || feature.contains("tower")
        || feature == "custom")
}

#[derive(Default)]
struct ToolpathBuilder {
    segments: Vec<(u32, Segment)>,
    position: [f32; 3],
    /// Start of the extrusion run not yet emitted as a segment.
    pending: Option<[f32; 3]>,
    layer: u32,
    layer_z: Option<f32>,
    /// Only layers divisible by this are kept; doubles whenever the budget is exceeded.
    stride: u32,
}

impl ToolpathBuilder {
    fn extrude_to(&mut self, point: [f32; 3]) {
        if self.layer_z != Some(point[2]) {
            self.flush();
            if self.layer_z.is_some() {
                self.layer += 1;
            }
            self.layer_z = Some(point[2]);
        }
        let start = *self.pending.get_or_insert(self.position);
        self.position = point;
        if distance(start, point) >= MIN_SEGMENT_MM {
            self.push([start, point]);
            self.pending = Some(point);
        }
    }

    fn move_to(&mut self, point: [f32; 3]) {
        self.flush();
        self.position = point;
    }

    fn flush(&mut self) {
        if let Some(start) = self.pending.take()
            && start != self.position
        {
            self.push([start, self.position]);
        }
    }

    fn push(&mut self, segment: Segment) {
        if !self.layer.is_multiple_of(self.stride) {
            return;
        }
        self.segments.push((self.layer, segment));
        while self.segments.len() > MAX_RENDER_SEGMENTS {
            self.stride *= 2;
            let stride = self.stride;
            self.segments
                .retain(|(layer, _)| layer.is_multiple_of(stride));
        }
    }
}

/// Extract extrusion toolpaths from slicer G-code, streaming line by line so
/// large prints never need to be held in memory.
fn parse_gcode(reader: impl BufRead) -> Result<Vec<Segment>> {
    let mut builder = ToolpathBuilder {
        stride: 1,
        ..Default::default()
    };
    let mut absolute_xyz = true;
    let mut relative_e = false;
    let mut last_e = 0.0_f32;
    let mut shown = false;

    for line in reader.split(b'\n') {
        let line = line.context("could not read 3MF G-code")?;
        let line = String::from_utf8_lossy(&line);
        let (code, comment) = line.split_once(';').unwrap_or((&line, ""));
        if let Some(feature) = comment.trim().strip_prefix("FEATURE:") {
            builder.flush();
            shown = feature_is_shown(feature.trim());
            continue;
        }
        let mut words = code.split_whitespace();
        let Some(command) = words.next() else {
            continue;
        };
        let mut params = [None::<f32>; 6];
        for word in words {
            let mut chars = word.chars();
            let slot = match chars.next().map(|c| c.to_ascii_uppercase()) {
                Some('X') => 0,
                Some('Y') => 1,
                Some('Z') => 2,
                Some('E') => 3,
                Some('I') => 4,
                Some('J') => 5,
                _ => continue,
            };
            params[slot] = chars.as_str().parse().ok();
        }
        let [x, y, z, e, i, j] = params;

        match command.to_ascii_uppercase().as_str() {
            "G90" => absolute_xyz = true,
            "G91" => absolute_xyz = false,
            "M82" => relative_e = false,
            "M83" => relative_e = true,
            "G92" => {
                if let Some(e) = e {
                    last_e = e;
                }
            }
            command @ ("G0" | "G1" | "G2" | "G3") => {
                let from = builder.position;
                let axis = |value: Option<f32>, index: usize| match value {
                    Some(value) if absolute_xyz => value,
                    Some(value) => from[index] + value,
                    None => from[index],
                };
                let target = [axis(x, 0), axis(y, 1), axis(z, 2)];
                let extruded = match e {
                    Some(e) if relative_e => e,
                    Some(e) => {
                        let delta = e - last_e;
                        last_e = e;
                        delta
                    }
                    None => 0.0,
                };
                if !(shown && extruded > 0.0 && command != "G0") || target == from {
                    builder.move_to(target);
                } else if command == "G1" {
                    builder.extrude_to(target);
                } else {
                    let center = [from[0] + i.unwrap_or(0.0), from[1] + j.unwrap_or(0.0)];
                    for point in arc_points(from, target, center, command == "G2") {
                        builder.extrude_to(point);
                    }
                }
            }
            _ => {}
        }
    }
    builder.flush();
    Ok(builder
        .segments
        .into_iter()
        .map(|(_, segment)| segment)
        .collect())
}

/// Flatten a G2 (clockwise) or G3 (counter-clockwise) arc into points, ending at `to`.
fn arc_points(
    from: [f32; 3],
    to: [f32; 3],
    center: [f32; 2],
    clockwise: bool,
) -> impl Iterator<Item = [f32; 3]> {
    use std::f32::consts::TAU;
    let radius = (from[0] - center[0]).hypot(from[1] - center[1]);
    let start = (from[1] - center[1]).atan2(from[0] - center[0]);
    let end = (to[1] - center[1]).atan2(to[0] - center[0]);
    let mut sweep = end - start;
    if clockwise && sweep >= 0.0 {
        sweep -= TAU;
    } else if !clockwise && sweep <= 0.0 {
        sweep += TAU;
    }
    let steps = ((sweep.abs() * radius / ARC_CHORD_MM).ceil() as usize).clamp(1, 360);
    (1..=steps).map(move |step| {
        if step == steps {
            return to;
        }
        let t = step as f32 / steps as f32;
        let angle = start + sweep * t;
        [
            center[0] + radius * angle.cos(),
            center[1] + radius * angle.sin(),
            from[2] + (to[2] - from[2]) * t,
        ]
    })
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[allow(dead_code)]
fn expand_object(
    id: u32,
    transform: Transform,
    depth: usize,
    objects: &HashMap<u32, ObjectDef>,
    output: &mut Vec<Triangle>,
) -> Result<()> {
    if depth > MAX_RECURSION {
        bail!("3MF component nesting is too deep");
    }
    match objects.get(&id) {
        Some(ObjectDef::Mesh {
            vertices,
            triangles,
        }) => {
            for indices in triangles {
                let Some(points) = indices
                    .iter()
                    .map(|index| vertices.get(*index as usize).copied())
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                output.push([
                    apply(transform, points[0]),
                    apply(transform, points[1]),
                    apply(transform, points[2]),
                ]);
                if output.len() > MAX_SOURCE_TRIANGLES {
                    bail!("expanded 3MF triangle count exceeds the safety limit");
                }
            }
        }
        Some(ObjectDef::Components(components)) => {
            for (child, child_transform) in components {
                expand_object(
                    *child,
                    compose(transform, *child_transform),
                    depth + 1,
                    objects,
                    output,
                )?;
            }
        }
        None => {}
    }
    Ok(())
}

#[allow(dead_code)]
fn attr_f32(node: roxmltree::Node<'_, '_>, name: &str) -> Result<f32> {
    node.attribute(name)
        .ok_or_else(|| anyhow!("3MF attribute {name} is missing"))?
        .parse::<f32>()
        .with_context(|| format!("invalid 3MF {name} coordinate"))
}

#[allow(dead_code)]
fn attr_u32(node: roxmltree::Node<'_, '_>, name: &str) -> Result<u32> {
    node.attribute(name)
        .ok_or_else(|| anyhow!("3MF attribute {name} is missing"))?
        .parse::<u32>()
        .with_context(|| format!("invalid 3MF {name} index"))
}

#[allow(dead_code)]
fn unit_scale(unit: &str) -> f32 {
    match unit {
        "micron" => 0.001,
        "centimeter" => 10.0,
        "meter" => 1_000.0,
        "inch" => 25.4,
        "foot" => 304.8,
        _ => 1.0,
    }
}

#[allow(dead_code)]
fn identity() -> Transform {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ]
}

#[allow(dead_code)]
fn parse_transform(value: Option<&str>) -> Transform {
    let Some(values) = value
        .map(|value| {
            value
                .split_whitespace()
                .filter_map(|part| part.parse::<f32>().ok())
                .collect::<Vec<_>>()
        })
        .filter(|values| values.len() == 12)
    else {
        return identity();
    };
    [
        [values[0], values[3], values[6], values[9]],
        [values[1], values[4], values[7], values[10]],
        [values[2], values[5], values[8], values[11]],
    ]
}

#[allow(dead_code)]
fn apply(transform: Transform, point: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|row| {
        transform[row][0] * point[0]
            + transform[row][1] * point[1]
            + transform[row][2] * point[2]
            + transform[row][3]
    })
}

#[allow(dead_code)]
fn compose(parent: Transform, child: Transform) -> Transform {
    let mut result = [[0.0; 4]; 3];
    for row in 0..3 {
        for column in 0..3 {
            result[row][column] = (0..3)
                .map(|index| parent[row][index] * child[index][column])
                .sum();
        }
        result[row][3] = parent[row][3]
            + (0..3)
                .map(|index| parent[row][index] * child[index][3])
                .sum::<f32>();
    }
    result
}

#[allow(dead_code)]
fn bounds<'a>(points: impl Iterator<Item = &'a [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for point in points {
        for axis in 0..3 {
            min[axis] = min[axis].min(point[axis]);
            max[axis] = max[axis].max(point[axis]);
        }
    }
    (min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_places_a_mesh() {
        let xml = r#"<?xml version="1.0"?><model unit="millimeter" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02"><resources><object id="1" type="model"><mesh><vertices><vertex x="0" y="0" z="0"/><vertex x="10" y="0" z="0"/><vertex x="0" y="10" z="5"/></vertices><triangles><triangle v1="0" v2="1" v3="2"/></triangles></mesh></object></resources><build><item objectid="1" transform="1 0 0 0 1 0 0 0 1 20 30 0"/></build></model>"#;
        let model = from_triangles("test.3mf".into(), mesh_triangles(xml).unwrap());
        assert_eq!(model.source_triangles, 1);
        assert_eq!(model.min, [20.0, 30.0, 0.0]);
        assert_eq!(model.max, [30.0, 40.0, 5.0]);
    }

    #[test]
    fn extracts_shown_gcode_toolpaths() {
        let gcode = "M83\n; FEATURE: Custom\nG1 X0 Y0 Z0.2\nG1 X50 Y0 E5\n\
            ; FEATURE: Outer wall\nG0 X10 Y10\nG1 X20 Y10 E1\n\
            G3 X10 Y10 I-5 J0 E1\n; FEATURE: Sparse infill\nG1 X20 Y20 E1\n";
        let segments = parse_gcode(gcode.as_bytes()).unwrap();
        // One straight wall plus a flattened half circle; purge line and infill skipped.
        assert!(segments.len() > 2);
        assert_eq!(segments[0], [[10.0, 10.0, 0.2], [20.0, 10.0, 0.2]]);
        assert_eq!(segments.last().unwrap()[1], [10.0, 10.0, 0.2]);
        let (min, max) = bounds(segments.iter().flatten());
        assert!(min[1] < 10.0 - 4.9 || max[1] > 10.0 + 4.9);
        assert!(max[0] <= 20.0 + 1e-3);
    }
}
