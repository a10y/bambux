use std::{collections::HashMap, io::Read};

use anyhow::{Context as _, Result, anyhow, bail};

const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_MODEL_XML_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_TRIANGLES: usize = 2_000_000;
const MAX_RENDER_TRIANGLES: usize = 12_000;
const MAX_RECURSION: usize = 16;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ModelMesh {
    pub name: String,
    pub triangles: Vec<Triangle>,
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub source_triangles: usize,
}

#[allow(dead_code)]
pub type Triangle = [[f32; 3]; 3];

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
    parse_model_xml(name, &xml)
}

#[allow(dead_code)]
fn parse_model_xml(name: String, xml: &str) -> Result<ModelMesh> {
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
    if expanded.is_empty() {
        bail!("3MF contains no renderable triangles");
    }

    let source_triangles = expanded.len();
    let triangles = if source_triangles > MAX_RENDER_TRIANGLES {
        let step = source_triangles as f64 / MAX_RENDER_TRIANGLES as f64;
        (0..MAX_RENDER_TRIANGLES)
            .map(|index| expanded[(index as f64 * step) as usize])
            .collect()
    } else {
        expanded
    };
    let (min, max) = bounds(&triangles);
    Ok(ModelMesh {
        name,
        triangles,
        min,
        max,
        source_triangles,
    })
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
fn bounds(triangles: &[Triangle]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for point in triangles.iter().flatten() {
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
        let model = parse_model_xml("test.3mf".into(), xml).unwrap();
        assert_eq!(model.source_triangles, 1);
        assert_eq!(model.min, [20.0, 30.0, 0.0]);
        assert_eq!(model.max, [30.0, 40.0, 5.0]);
    }
}
