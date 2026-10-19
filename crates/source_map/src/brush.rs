//! Hammer solids: planes from three points, face polygons from the planes.

use crate::keyvalues::Block;

pub type Vec3 = [f32; 3];

pub fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

pub fn length(a: Vec3) -> f32 {
    dot(a, a).sqrt()
}

pub fn normalize(a: Vec3) -> Vec3 {
    let len = length(a);
    if len <= 1e-12 {
        [0.0, 0.0, 1.0]
    } else {
        scale(a, 1.0 / len)
    }
}

pub fn lerp(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    add(a, scale(sub(b, a), t))
}

/// `[x y z offset] scale`: texels along `dir` are `dot(p, dir) / scale + offset`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TexAxis {
    pub dir: Vec3,
    pub offset: f32,
    pub scale: f32,
}

impl TexAxis {
    fn parse(text: &str) -> Result<Self, String> {
        let (bracket, rest) = text
            .trim()
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .ok_or_else(|| format!("texture axis `{text}`"))?;
        let parts = floats(bracket)?;
        if parts.len() != 4 {
            return Err(format!("texture axis `{text}`: expected 4 numbers"));
        }
        let scale: f32 = rest
            .trim()
            .parse()
            .map_err(|_| format!("texture axis `{text}`: scale"))?;
        Ok(Self {
            dir: [parts[0], parts[1], parts[2]],
            offset: parts[3],
            scale: if scale.abs() < 1e-6 { 0.25 } else { scale },
        })
    }
}

pub fn floats(text: &str) -> Result<Vec<f32>, String> {
    text.split_whitespace()
        .map(|word| {
            word.parse::<f32>()
                .map_err(|_| format!("`{word}` in `{text}` is not a number"))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Side {
    pub id: u32,
    /// Outward normal and distance: `dot(normal, p) == dist` on the plane,
    /// the brush interior on the negative side.
    pub plane: [f32; 4],
    pub material: String,
    pub uaxis: TexAxis,
    pub vaxis: TexAxis,
    pub lightmap_scale: f32,
    pub disp: Option<crate::disp::DispInfo>,
    /// The face polygon, counter-clockwise seen from outside; empty when the
    /// other planes clipped it away entirely.
    pub winding: Vec<Vec3>,
}

#[derive(Clone, Debug)]
pub struct Brush {
    pub id: u32,
    pub sides: Vec<Side>,
    pub mins: Vec3,
    pub maxs: Vec3,
}

/// The plane through three points listed clockwise seen from the front.
pub fn plane_from_points(text: &str) -> Result<[f32; 4], String> {
    let cleaned = text.replace(['(', ')'], " ");
    let numbers = floats(&cleaned)?;
    if numbers.len() != 9 {
        return Err(format!("plane `{text}`: expected 3 points"));
    }
    let p1 = [numbers[0], numbers[1], numbers[2]];
    let p2 = [numbers[3], numbers[4], numbers[5]];
    let p3 = [numbers[6], numbers[7], numbers[8]];
    let normal = cross(sub(p3, p1), sub(p2, p1));
    if length(normal) <= 1e-6 {
        return Err(format!("plane `{text}`: points are colinear"));
    }
    let normal = normalize(normal);
    Ok([normal[0], normal[1], normal[2], dot(normal, p1)])
}

const WINDING_EXTENT: f32 = 131_072.0;
const ON_EPSILON: f32 = 0.01;

/// A huge square on the plane, counter-clockwise around its normal.
pub fn base_winding(plane: [f32; 4]) -> Vec<Vec3> {
    let normal = [plane[0], plane[1], plane[2]];
    let helper = if normal[2].abs() < 0.9 {
        [0.0, 0.0, 1.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let right = normalize(cross(helper, normal));
    let up = normalize(cross(normal, right));
    let center = scale(normal, plane[3]);
    let r = scale(right, WINDING_EXTENT);
    let u = scale(up, WINDING_EXTENT);
    let mut winding = vec![
        add(add(center, r), u),
        add(sub(center, r), u),
        sub(sub(center, r), u),
        sub(add(center, r), u),
    ];
    let test = cross(sub(winding[1], winding[0]), sub(winding[2], winding[0]));
    if dot(test, normal) < 0.0 {
        winding.reverse();
    }
    winding
}

/// Keeps the part of `winding` behind `plane` (`dot(n, p) - d <= 0`).
pub fn clip_winding(winding: &[Vec3], plane: [f32; 4]) -> Vec<Vec3> {
    let normal = [plane[0], plane[1], plane[2]];
    let mut out = Vec::with_capacity(winding.len() + 2);
    let n = winding.len();
    for i in 0..n {
        let a = winding[i];
        let b = winding[(i + 1) % n];
        let da = dot(normal, a) - plane[3];
        let db = dot(normal, b) - plane[3];
        let a_in = da <= ON_EPSILON;
        let b_in = db <= ON_EPSILON;
        if a_in {
            out.push(a);
        }
        if a_in != b_in && (da - db).abs() > 1e-9 {
            let t = da / (da - db);
            out.push(lerp(a, b, t));
        }
    }
    dedup_winding(out)
}

fn dedup_winding(points: Vec<Vec3>) -> Vec<Vec3> {
    let mut out: Vec<Vec3> = Vec::with_capacity(points.len());
    for p in points {
        if out
            .last()
            .is_some_and(|last| length(sub(*last, p)) < 0.05)
        {
            continue;
        }
        out.push(p);
    }
    while out.len() > 1 && length(sub(out[0], out[out.len() - 1])) < 0.05 {
        out.pop();
    }
    if out.len() < 3 { Vec::new() } else { out }
}

pub fn parse_brush(block: &Block) -> Result<Brush, String> {
    let id = block
        .get("id")
        .and_then(|id| id.parse().ok())
        .unwrap_or(0);
    let mut sides = Vec::new();
    for side in block.children_named("side") {
        let plane_text = side
            .get("plane")
            .ok_or_else(|| format!("solid {id}: side without a plane"))?;
        let plane = plane_from_points(plane_text).map_err(|e| format!("solid {id}: {e}"))?;
        let material = side
            .get("material")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .replace('\\', "/");
        let uaxis = TexAxis::parse(side.get("uaxis").unwrap_or("[1 0 0 0] 0.25"))
            .map_err(|e| format!("solid {id}: {e}"))?;
        let vaxis = TexAxis::parse(side.get("vaxis").unwrap_or("[0 -1 0 0] 0.25"))
            .map_err(|e| format!("solid {id}: {e}"))?;
        let lightmap_scale = side
            .get("lightmapscale")
            .and_then(|s| s.parse().ok())
            .unwrap_or(16.0);
        let disp = match side.child("dispinfo") {
            Some(info) => {
                Some(crate::disp::parse_disp(info).map_err(|e| format!("solid {id}: {e}"))?)
            }
            None => None,
        };
        sides.push(Side {
            id: side.get("id").and_then(|s| s.parse().ok()).unwrap_or(0),
            plane,
            material,
            uaxis,
            vaxis,
            lightmap_scale,
            disp,
            winding: Vec::new(),
        });
    }
    if sides.len() < 4 {
        return Err(format!("solid {id}: {} sides", sides.len()));
    }
    let planes: Vec<[f32; 4]> = sides.iter().map(|s| s.plane).collect();
    let mut mins = [f32::INFINITY; 3];
    let mut maxs = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for (index, side) in sides.iter_mut().enumerate() {
        let mut winding = base_winding(side.plane);
        for (other, plane) in planes.iter().enumerate() {
            if other == index {
                continue;
            }
            winding = clip_winding(&winding, *plane);
            if winding.is_empty() {
                break;
            }
        }
        for p in &winding {
            for axis in 0..3 {
                mins[axis] = mins[axis].min(p[axis]);
                maxs[axis] = maxs[axis].max(p[axis]);
            }
            any = true;
        }
        side.winding = winding;
    }
    if !any {
        return Err(format!("solid {id}: no volume"));
    }
    Ok(Brush {
        id,
        sides,
        mins,
        maxs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_normal_points_out_of_the_box() {
        // The x = 17 face of a box spanning x 17..64, as Hammer wrote it.
        let plane = plane_from_points("(17 -1828 -64) (17 -1828 -168) (17 -1783 -168)").unwrap();
        assert!((plane[0] + 1.0).abs() < 1e-5, "{plane:?}");
        assert!((plane[3] + 17.0).abs() < 1e-3, "{plane:?}");
    }

    #[test]
    fn box_brush_has_six_quads() {
        let text = r#"
solid
{
	"id" "2"
	side { "id" "3" "plane" "(17 -1828 -64) (17 -1828 -168) (17 -1783 -168)" "material" "a" "uaxis" "[0 1 0 -56] 0.25" "vaxis" "[0 0 -1 0] 0.25" }
	side { "id" "4" "plane" "(64 -1783 -64) (64 -1783 -168) (64 -1828 -168)" "material" "a" "uaxis" "[0 1 0 -56] 0.25" "vaxis" "[0 0 -1 0] 0.25" }
	side { "id" "5" "plane" "(64 -1828 -64) (64 -1828 -168) (17 -1828 -168)" "material" "a" "uaxis" "[-1 0 0 0] 0.25" "vaxis" "[0 0 -1 0] 0.25" }
	side { "id" "6" "plane" "(17 -1783 -64) (17 -1783 -168) (64 -1783 -168)" "material" "a" "uaxis" "[1 0 -0 0] 0.25" "vaxis" "[0 -0 -1 0] 0.25" }
	side { "id" "7" "plane" "(17 -1783 -168) (17 -1828 -168) (64 -1828 -168)" "material" "a" "uaxis" "[0 -1 0 0] 0.25" "vaxis" "[-1 0 0 0] 0.25" }
	side { "id" "8" "plane" "(17 -1828 -64) (17 -1783 -64) (64 -1783 -64)" "material" "a" "uaxis" "[-0 1 0 0] 0.25" "vaxis" "[-1 0 -0 0] 0.25" }
}
"#;
        let block = crate::keyvalues::parse(text).unwrap().remove(0);
        let brush = parse_brush(&block).unwrap();
        assert_eq!(brush.mins, [17.0, -1828.0, -168.0]);
        assert_eq!(brush.maxs, [64.0, -1783.0, -64.0]);
        for side in &brush.sides {
            assert_eq!(side.winding.len(), 4, "side {}: {:?}", side.id, side.winding);
            let n = [side.plane[0], side.plane[1], side.plane[2]];
            let w = &side.winding;
            let face = cross(sub(w[1], w[0]), sub(w[2], w[0]));
            assert!(dot(face, n) > 0.0, "side {} winds against its normal", side.id);
            let centroid = [40.5, -1805.5, -116.0];
            assert!(
                dot(n, centroid) - side.plane[3] < 0.0,
                "side {} faces inward",
                side.id
            );
        }
    }
}
