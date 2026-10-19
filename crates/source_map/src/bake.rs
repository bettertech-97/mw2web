//! A lightmap bake for a compiled map: every drawn face gets a chart in a
//! page, and every texel of it a sun visibility (shadows) and an ambient
//! term (sky light with occlusion). Rays are cast against the map's own
//! drawn triangles under a bounding-volume tree. The result is written to
//! `<zone>.lightmap.bin` beside the map and read back by the engine, which
//! keeps the same face order, so charts are keyed by face index.

use crate::brush::{Vec3, cross, dot, normalize, scale, sub};
use crate::compile::CompiledMap;

/// Version 3: charts, then per-face sun bits and per-vertex sky openness
/// for props, then the page texels as one zlib stream.
const MAGIC: &[u8; 8] = b"IW4LLMP3";
/// Page 0 keeps a 16×16 corner for props: the left half fully sunlit, the
/// right half in shadow, both under open sky. A prop face points its
/// lightmap coordinates at one of these two texels and carries its
/// occlusion in vertex colour.
pub const PROP_TEXEL_LIT: [u32; 2] = [4, 8];
pub const PROP_TEXEL_SHADOW: [u32; 2] = [12, 8];
const PROP_RESERVE: u32 = 16;
/// Texels of padding around each chart, so bilinear sampling at a face's
/// edge never reads a neighbour.
const PAD: u32 = 1;
const SUN_SAMPLES: usize = 16;
const AMBIENT_SAMPLES: usize = 32;
const AMBIENT_REACH: f32 = 384.0;
const SUN_DISC: f32 = 0.025;

#[derive(Clone, Debug, PartialEq)]
pub struct Chart {
    pub page: u16,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub origin: Vec3,
    pub u_axis: Vec3,
    pub v_axis: Vec3,
}

impl Chart {
    /// Lightmap coordinates of a world position on this chart's plane.
    pub fn uv(&self, position: Vec3, texel: f32, page_size: u32) -> [f32; 2] {
        let d = sub(position, self.origin);
        let u = (dot(d, self.u_axis) / texel + PAD as f32 + f32::from(self.x)) / page_size as f32;
        let v = (dot(d, self.v_axis) / texel + PAD as f32 + f32::from(self.y)) / page_size as f32;
        [u, v]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// Sun visibility per texel, 255 = fully lit.
    pub sun: Vec<u8>,
    /// How open the sky is per texel, 0..255: the share of hemisphere
    /// rays that escaped.
    pub ambient: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bake {
    pub texel: f32,
    pub page_size: u32,
    pub pages: Vec<Page>,
    /// One per drawn face, in `CompiledMap::surfaces` order; `None` for sky
    /// and for props.
    pub charts: Vec<Option<Chart>>,
    /// Per face: 0 for a world face, 1 for a prop face in shadow, 2 for a
    /// prop face in the sun.
    pub face_sun: Vec<u8>,
    /// Per vertex: how open the sky is at a prop vertex, 0..255 (0 for
    /// world vertices, which read their chart instead).
    pub vertex_open: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BakeStats {
    pub faces: usize,
    pub texels: usize,
    pub pages: usize,
    pub triangles: usize,
    pub rays: usize,
}

struct Triangle {
    v0: Vec3,
    e1: Vec3,
    e2: Vec3,
}

struct Node {
    mins: Vec3,
    maxs: Vec3,
    /// Leaf: `first..first + count` into the triangle order; else `first`
    /// is the left child and `first + 1` the right.
    first: u32,
    count: u32,
}

struct Bvh {
    triangles: Vec<Triangle>,
    order: Vec<u32>,
    nodes: Vec<Node>,
}

impl Bvh {
    fn build(triangles: Vec<Triangle>) -> Self {
        let centroids: Vec<Vec3> = triangles
            .iter()
            .map(|t| {
                let a = t.v0;
                let b = [a[0] + t.e1[0], a[1] + t.e1[1], a[2] + t.e1[2]];
                let c = [a[0] + t.e2[0], a[1] + t.e2[1], a[2] + t.e2[2]];
                [
                    (a[0] + b[0] + c[0]) / 3.0,
                    (a[1] + b[1] + c[1]) / 3.0,
                    (a[2] + b[2] + c[2]) / 3.0,
                ]
            })
            .collect();
        let mut order: Vec<u32> = (0..triangles.len() as u32).collect();
        let mut nodes = Vec::with_capacity(triangles.len() / 4 + 1);
        nodes.push(Node {
            mins: [0.0; 3],
            maxs: [0.0; 3],
            first: 0,
            count: 0,
        });
        let mut bvh = Self {
            triangles,
            order,
            nodes,
        };
        let count = bvh.order.len();
        bvh.subdivide(0, 0, count, &centroids);
        order = Vec::new();
        let _ = order;
        bvh
    }

    fn bounds(&self, start: usize, end: usize) -> (Vec3, Vec3) {
        let mut mins = [f32::INFINITY; 3];
        let mut maxs = [f32::NEG_INFINITY; 3];
        for &index in &self.order[start..end] {
            let t = &self.triangles[index as usize];
            for p in [
                t.v0,
                [t.v0[0] + t.e1[0], t.v0[1] + t.e1[1], t.v0[2] + t.e1[2]],
                [t.v0[0] + t.e2[0], t.v0[1] + t.e2[1], t.v0[2] + t.e2[2]],
            ] {
                for axis in 0..3 {
                    mins[axis] = mins[axis].min(p[axis]);
                    maxs[axis] = maxs[axis].max(p[axis]);
                }
            }
        }
        (mins, maxs)
    }

    fn subdivide(&mut self, node: usize, start: usize, end: usize, centroids: &[Vec3]) {
        let (mins, maxs) = self.bounds(start, end);
        self.nodes[node].mins = mins;
        self.nodes[node].maxs = maxs;
        if end - start <= 6 {
            self.nodes[node].first = start as u32;
            self.nodes[node].count = (end - start) as u32;
            return;
        }
        let mut cmins = [f32::INFINITY; 3];
        let mut cmaxs = [f32::NEG_INFINITY; 3];
        for &index in &self.order[start..end] {
            let c = centroids[index as usize];
            for axis in 0..3 {
                cmins[axis] = cmins[axis].min(c[axis]);
                cmaxs[axis] = cmaxs[axis].max(c[axis]);
            }
        }
        let axis = (0..3)
            .max_by(|&a, &b| (cmaxs[a] - cmins[a]).total_cmp(&(cmaxs[b] - cmins[b])))
            .unwrap_or(0);
        self.order[start..end].sort_by(|&a, &b| {
            centroids[a as usize][axis].total_cmp(&centroids[b as usize][axis])
        });
        let mid = (start + end) / 2;
        let left = self.nodes.len();
        self.nodes.push(Node {
            mins: [0.0; 3],
            maxs: [0.0; 3],
            first: 0,
            count: 0,
        });
        self.nodes.push(Node {
            mins: [0.0; 3],
            maxs: [0.0; 3],
            first: 0,
            count: 0,
        });
        self.nodes[node].first = left as u32;
        self.nodes[node].count = 0;
        self.subdivide(left, start, mid, centroids);
        self.subdivide(left + 1, mid, end, centroids);
    }

    /// Whether anything lies along `origin + t * direction` for `t` in
    /// `(0, max_t)`. Both triangle sides count.
    fn occluded(&self, origin: Vec3, direction: Vec3, max_t: f32) -> bool {
        let inv = [
            1.0 / if direction[0] == 0.0 { 1e-9 } else { direction[0] },
            1.0 / if direction[1] == 0.0 { 1e-9 } else { direction[1] },
            1.0 / if direction[2] == 0.0 { 1e-9 } else { direction[2] },
        ];
        let mut stack = [0usize; 64];
        let mut depth = 1;
        stack[0] = 0;
        while depth > 0 {
            depth -= 1;
            let node = &self.nodes[stack[depth]];
            let mut t_enter = 0.0f32;
            let mut t_exit = max_t;
            let mut miss = false;
            for axis in 0..3 {
                let t0 = (node.mins[axis] - origin[axis]) * inv[axis];
                let t1 = (node.maxs[axis] - origin[axis]) * inv[axis];
                let (near, far) = if t0 < t1 { (t0, t1) } else { (t1, t0) };
                t_enter = t_enter.max(near);
                t_exit = t_exit.min(far);
                if t_enter > t_exit {
                    miss = true;
                    break;
                }
            }
            if miss {
                continue;
            }
            if node.count > 0 {
                let first = node.first as usize;
                for &index in &self.order[first..first + node.count as usize] {
                    if hit_triangle(&self.triangles[index as usize], origin, direction, max_t) {
                        return true;
                    }
                }
            } else if depth + 2 <= stack.len() {
                stack[depth] = node.first as usize;
                stack[depth + 1] = node.first as usize + 1;
                depth += 2;
            }
        }
        false
    }
}

fn hit_triangle(t: &Triangle, origin: Vec3, direction: Vec3, max_t: f32) -> bool {
    let p = cross(direction, t.e2);
    let det = dot(t.e1, p);
    if det.abs() < 1e-7 {
        return false;
    }
    let inv = 1.0 / det;
    let s = sub(origin, t.v0);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = cross(s, t.e1);
    let v = dot(direction, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    let hit = dot(t.e2, q) * inv;
    hit > 0.05 && hit < max_t
}

/// A deterministic hash-based pseudo random stream, so bakes repeat.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn chart_basis(normal: Vec3) -> (Vec3, Vec3) {
    let least = (0..3)
        .min_by(|&a, &b| normal[a].abs().total_cmp(&normal[b].abs()))
        .unwrap_or(0);
    let mut axis = [0.0; 3];
    axis[least] = 1.0;
    let u = normalize(cross(normal, axis));
    let v = normalize(cross(normal, u));
    (u, v)
}

/// A shelf packer: rows of charts left to right, top to bottom.
struct Shelf {
    size: u32,
    x: u32,
    y: u32,
    row_height: u32,
}

impl Shelf {
    fn new(size: u32) -> Self {
        Self {
            size,
            x: 0,
            y: 0,
            row_height: 0,
        }
    }

    fn place(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width > self.size || height > self.size {
            return None;
        }
        if self.x + width > self.size {
            self.y += self.row_height;
            self.x = 0;
            self.row_height = 0;
        }
        if self.y + height > self.size {
            return None;
        }
        let at = (self.x, self.y);
        self.x += width;
        self.row_height = self.row_height.max(height);
        Some(at)
    }
}

/// Bakes `map` at `texel` units per lightmap texel into pages of
/// `page_size` texels a side, running `threads` workers.
pub fn bake(map: &CompiledMap, texel: f32, page_size: u32, threads: usize) -> (Bake, BakeStats) {
    let mut stats = BakeStats::default();
    // Occluders: every drawn face but the sky.
    let mut triangles = Vec::with_capacity(map.indices.len() / 3);
    for face in map.surfaces.iter().filter(|f| !f.sky) {
        let first = face.first_index as usize;
        for tri in map.indices[first..first + face.index_count as usize].chunks_exact(3) {
            let a = map.vertices[tri[0] as usize].position;
            let b = map.vertices[tri[1] as usize].position;
            let c = map.vertices[tri[2] as usize].position;
            triangles.push(Triangle {
                v0: a,
                e1: sub(b, a),
                e2: sub(c, a),
            });
        }
    }
    stats.triangles = triangles.len();
    let bvh = Bvh::build(triangles);

    // Charts, packed face by face in spatial order so a page covers a
    // neighbourhood.
    let mut order: Vec<usize> = (0..map.surfaces.len()).collect();
    let cell = |face: &crate::compile::Surface| {
        (
            (face.mins[0] / 1024.0).floor() as i32,
            (face.mins[1] / 1024.0).floor() as i32,
        )
    };
    order.sort_by_key(|&i| cell(&map.surfaces[i]));
    let mut charts: Vec<Option<Chart>> = vec![None; map.surfaces.len()];
    let mut shelf = Shelf::new(page_size);
    let mut page_index = 0u16;
    shelf.place(PROP_RESERVE, PROP_RESERVE);
    for &face_index in &order {
        let face = &map.surfaces[face_index];
        if face.sky || face.prop || face.vertex_count == 0 {
            continue;
        }
        let normal = normalize(face.normal);
        let (u_axis, v_axis) = chart_basis(normal);
        let first = face.first_vertex as usize;
        let verts = &map.vertices[first..first + face.vertex_count as usize];
        let (mut umin, mut umax, mut vmin, mut vmax) =
            (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
        for v in verts {
            let u = dot(v.position, u_axis);
            let w = dot(v.position, v_axis);
            umin = umin.min(u);
            umax = umax.max(u);
            vmin = vmin.min(w);
            vmax = vmax.max(w);
        }
        let width = (((umax - umin) / texel).ceil() as u32).max(1) + 1;
        let height = (((vmax - vmin) / texel).ceil() as u32).max(1) + 1;
        let (full_w, full_h) = (width + 2 * PAD, height + 2 * PAD);
        if full_w > page_size || full_h > page_size {
            continue;
        }
        let at = match shelf.place(full_w, full_h) {
            Some(at) => at,
            None => {
                shelf = Shelf::new(page_size);
                page_index += 1;
                shelf.place(full_w, full_h).unwrap_or((0, 0))
            }
        };
        // The origin sits on the face plane at the chart's corner.
        let plane_d = dot(normal, verts[0].position);
        let origin_flat = [
            u_axis[0] * umin + v_axis[0] * vmin,
            u_axis[1] * umin + v_axis[1] * vmin,
            u_axis[2] * umin + v_axis[2] * vmin,
        ];
        let lift = plane_d - dot(normal, origin_flat);
        let origin = [
            origin_flat[0] + normal[0] * lift,
            origin_flat[1] + normal[1] * lift,
            origin_flat[2] + normal[2] * lift,
        ];
        charts[face_index] = Some(Chart {
            page: page_index,
            x: at.0 as u16,
            y: at.1 as u16,
            width: width as u16,
            height: height as u16,
            origin,
            u_axis,
            v_axis,
        });
        stats.faces += 1;
        stats.texels += (width * height) as usize;
    }
    let page_count = usize::from(page_index) + 1;
    stats.pages = page_count;

    let sun = map.sun.as_ref().map(|s| normalize(s.to_sun));
    let texels_per_page = (page_size * page_size) as usize;
    let mut pages: Vec<Page> = (0..page_count)
        .map(|_| Page {
            sun: vec![0u8; texels_per_page],
            ambient: vec![0u8; texels_per_page],
        })
        .collect();
    for y in 0..PROP_RESERVE.min(page_size) {
        for x in 0..PROP_RESERVE.min(page_size) {
            let at = (y * page_size + x) as usize;
            pages[0].sun[at] = if x < PROP_RESERVE / 2 { 255 } else { 0 };
            pages[0].ambient[at] = 255;
        }
    }

    // Every chart's texels, baked in parallel and written back per face.
    let work: Vec<usize> = (0..map.surfaces.len()).filter(|&i| charts[i].is_some()).collect();
    let threads = threads.max(1);
    let chunk = work.len().div_ceil(threads).max(1);
    let results: Vec<Vec<(usize, Vec<u8>, Vec<u8>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = work
            .chunks(chunk)
            .map(|faces| {
                let bvh = &bvh;
                let charts = &charts;
                scope.spawn(move || {
                    let mut out = Vec::with_capacity(faces.len());
                    for &face_index in faces {
                        let chart = charts[face_index].as_ref().unwrap();
                        let (s, a) = bake_face(map, face_index, chart, texel, sun, bvh);
                        out.push((face_index, s, a));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    for result in results {
        for (face_index, sun_texels, ambient_texels) in result {
            let chart = charts[face_index].as_ref().unwrap();
            let page = &mut pages[usize::from(chart.page)];
            let (w, h) = (u32::from(chart.width), u32::from(chart.height));
            // Write the chart with its padding filled from the nearest edge
            // texel.
            for py in 0..h + 2 * PAD {
                for px in 0..w + 2 * PAD {
                    let sx = px.saturating_sub(PAD).min(w - 1);
                    let sy = py.saturating_sub(PAD).min(h - 1);
                    let from = (sy * w + sx) as usize;
                    let to = ((u32::from(chart.y) + py) * page_size + u32::from(chart.x) + px) as usize;
                    if to < texels_per_page {
                        page.sun[to] = sun_texels[from];
                        page.ambient[to] = ambient_texels[from];
                    }
                }
            }
            stats.rays += (w * h) as usize * (SUN_SAMPLES + AMBIENT_SAMPLES);
        }
    }
    // Props: a sun and sky sample per vertex, in parallel, then a sun bit
    // per face from its vertices' average.
    let mut is_prop_vertex = vec![false; map.vertices.len()];
    for face in map.surfaces.iter().filter(|f| f.prop) {
        let first = face.first_vertex as usize;
        for flag in is_prop_vertex.iter_mut().skip(first).take(face.vertex_count as usize) {
            *flag = true;
        }
    }
    let prop_vertices: Vec<usize> = (0..map.vertices.len()).filter(|&i| is_prop_vertex[i]).collect();
    let mut vertex_sun = vec![0u8; map.vertices.len()];
    let mut vertex_open = vec![0u8; map.vertices.len()];
    if !prop_vertices.is_empty() {
        let chunk = prop_vertices.len().div_ceil(threads).max(1);
        let results: Vec<Vec<(usize, u8, u8)>> = std::thread::scope(|scope| {
            let handles: Vec<_> = prop_vertices
                .chunks(chunk)
                .map(|vertices| {
                    let bvh = &bvh;
                    scope.spawn(move || {
                        let mut rng = Rng(0x2545_F491_4F6C_DD1D ^ (vertices[0] as u64 + 7).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                        vertices
                            .iter()
                            .map(|&index| {
                                let v = &map.vertices[index];
                                let (s, o) = sample_point(v.position, normalize(v.normal), sun, bvh, &mut rng);
                                (index, s, o)
                            })
                            .collect()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        for result in results {
            for (index, s, o) in result {
                vertex_sun[index] = s;
                vertex_open[index] = o;
            }
        }
        stats.rays += prop_vertices.len() * (SUN_SAMPLES + AMBIENT_SAMPLES);
    }
    let mut face_sun = vec![0u8; map.surfaces.len()];
    for (face_index, face) in map.surfaces.iter().enumerate() {
        if !face.prop {
            continue;
        }
        let first = face.first_index as usize;
        let used = &map.indices[first..first + face.index_count as usize];
        let (mut total, mut count) = (0u32, 0u32);
        for &i in used {
            total += u32::from(vertex_sun[i as usize]);
            count += 1;
        }
        face_sun[face_index] = if count > 0 && total >= count * 128 { 2 } else { 1 };
    }
    (
        Bake {
            texel,
            page_size,
            pages,
            charts,
            face_sun,
            vertex_open,
        },
        stats,
    )
}

/// Sun visibility and sky openness at one point, both 0..255.
fn sample_point(position: Vec3, normal: Vec3, sun: Option<Vec3>, bvh: &Bvh, rng: &mut Rng) -> (u8, u8) {
    let start = [
        position[0] + normal[0] * 1.0,
        position[1] + normal[1] * 1.0,
        position[2] + normal[2] * 1.0,
    ];
    let (t_axis, b_axis) = chart_basis(normal);
    let mut lit = 0.0f32;
    if let Some(sun) = sun
        && dot(normal, sun) > 0.0
    {
        for _ in 0..SUN_SAMPLES {
            let dx = (rng.next() - 0.5) * 2.0 * SUN_DISC;
            let dy = (rng.next() - 0.5) * 2.0 * SUN_DISC;
            let direction = normalize([
                sun[0] + t_axis[0] * dx + b_axis[0] * dy,
                sun[1] + t_axis[1] * dx + b_axis[1] * dy,
                sun[2] + t_axis[2] * dx + b_axis[2] * dy,
            ]);
            if !bvh.occluded(start, direction, 1.0e5) {
                lit += 1.0 / SUN_SAMPLES as f32;
            }
        }
    }
    let mut open = 0.0f32;
    for _ in 0..AMBIENT_SAMPLES {
        let r1 = rng.next();
        let r2 = rng.next();
        let radius = r1.sqrt();
        let angle = r2 * std::f32::consts::TAU;
        let (sx, sy) = (radius * angle.cos(), radius * angle.sin());
        let sz = (1.0 - r1).max(0.0).sqrt();
        let direction = normalize([
            t_axis[0] * sx + b_axis[0] * sy + normal[0] * sz,
            t_axis[1] * sx + b_axis[1] * sy + normal[1] * sz,
            t_axis[2] * sx + b_axis[2] * sy + normal[2] * sz,
        ]);
        if !bvh.occluded(start, direction, AMBIENT_REACH) {
            open += 1.0;
        }
    }
    (
        (lit * 255.0).round().clamp(0.0, 255.0) as u8,
        (open / AMBIENT_SAMPLES as f32 * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// The world position of a chart texel: on the face (its triangles in chart
/// space: the face plane, or the displaced mesh). A texel past the face's
/// edge (the chart is the face's bounding rectangle) takes the nearest point
/// of the face, a little inside: lit from beyond the edge it sat inside the
/// next wall or under the floor, came out black, and bilinear filtering
/// smeared that into the face as dark saw-tooth wedges.
fn texel_position(
    map: &CompiledMap,
    face_index: usize,
    chart: &Chart,
    texel: f32,
    px: u32,
    py: u32,
    tris2d: &[([f32; 2], [f32; 2], [f32; 2], Vec3, Vec3, Vec3, Vec3)],
) -> (Vec3, Vec3) {
    let face = &map.surfaces[face_index];
    let normal = normalize(face.normal);
    let cu = (px as f32 + 0.5) * texel;
    let cv = (py as f32 + 0.5) * texel;
    let flat = [
        chart.origin[0] + chart.u_axis[0] * cu + chart.v_axis[0] * cv,
        chart.origin[1] + chart.u_axis[1] * cu + chart.v_axis[1] * cv,
        chart.origin[2] + chart.u_axis[2] * cu + chart.v_axis[2] * cv,
    ];
    let mut best: Option<(f32, [f32; 3], usize)> = None;
    for (index, (a, b, c, pa, pb, pc, n)) in tris2d.iter().enumerate() {
        let d = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
        if d.abs() < 1e-9 {
            continue;
        }
        let l0 = ((b[1] - c[1]) * (cu - c[0]) + (c[0] - b[0]) * (cv - c[1])) / d;
        let l1 = ((c[1] - a[1]) * (cu - c[0]) + (a[0] - c[0]) * (cv - c[1])) / d;
        let l2 = 1.0 - l0 - l1;
        let outside = l0.min(l1).min(l2);
        if outside >= -1e-4 {
            let position = if face.displacement {
                [
                    pa[0] * l0 + pb[0] * l1 + pc[0] * l2,
                    pa[1] * l0 + pb[1] * l1 + pc[1] * l2,
                    pa[2] * l0 + pb[2] * l1 + pc[2] * l2,
                ]
            } else {
                flat
            };
            return (position, if face.displacement { *n } else { normal });
        }
        if best.is_none_or(|(o, _, _)| outside > o) {
            best = Some((outside, [l0, l1, l2], index));
        }
    }
    let Some((_, weights, index)) = best else {
        return (flat, normal);
    };
    // The nearest triangle's point, its weights clamped onto it and then
    // drawn a tenth of the way to its middle.
    let (_, _, _, pa, pb, pc, n) = &tris2d[index];
    let clamped = weights.map(|w| w.max(0.0));
    let sum = clamped.iter().sum::<f32>().max(1e-6);
    let w = clamped.map(|w| w / sum * 0.9 + 0.1 / 3.0);
    let position = [
        pa[0] * w[0] + pb[0] * w[1] + pc[0] * w[2],
        pa[1] * w[0] + pb[1] * w[1] + pc[1] * w[2],
        pa[2] * w[0] + pb[2] * w[1] + pc[2] * w[2],
    ];
    (position, if face.displacement { *n } else { normal })
}

fn bake_face(
    map: &CompiledMap,
    face_index: usize,
    chart: &Chart,
    texel: f32,
    sun: Option<Vec3>,
    bvh: &Bvh,
) -> (Vec<u8>, Vec<u8>) {
    let face = &map.surfaces[face_index];
    let (w, h) = (u32::from(chart.width), u32::from(chart.height));
    let mut sun_texels = vec![0u8; (w * h) as usize];
    let mut ambient_texels = vec![0u8; (w * h) as usize];
    // The face's triangles in chart space, with their world corners and
    // normals: what each texel is placed on.
    let first = face.first_index as usize;
    let tris2d: Vec<([f32; 2], [f32; 2], [f32; 2], Vec3, Vec3, Vec3, Vec3)> = map.indices
        [first..first + face.index_count as usize]
        .chunks_exact(3)
        .map(|tri| {
            let p = [
                map.vertices[tri[0] as usize].position,
                map.vertices[tri[1] as usize].position,
                map.vertices[tri[2] as usize].position,
            ];
            let to2d = |q: Vec3| {
                let d = sub(q, chart.origin);
                [dot(d, chart.u_axis), dot(d, chart.v_axis)]
            };
            let n = normalize(cross(sub(p[1], p[0]), sub(p[2], p[0])));
            let vn = map.vertices[tri[0] as usize].normal;
            let n = if dot(n, vn) < 0.0 { scale(n, -1.0) } else { n };
            (to2d(p[0]), to2d(p[1]), to2d(p[2]), p[0], p[1], p[2], n)
        })
        .collect();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (face_index as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    for py in 0..h {
        for px in 0..w {
            let (position, normal) = texel_position(map, face_index, chart, texel, px, py, &tris2d);
            let start = [
                position[0] + normal[0] * 1.0,
                position[1] + normal[1] * 1.0,
                position[2] + normal[2] * 1.0,
            ];
            let (t_axis, b_axis) = chart_basis(normal);
            // Sun: a few rays over a small disc for soft edges; nothing
            // reaches a texel facing away from it.
            let mut lit = 0.0f32;
            if let Some(sun) = sun {
                let facing = dot(normal, sun);
                if facing > 0.0 {
                    for _ in 0..SUN_SAMPLES {
                        let dx = (rng.next() - 0.5) * 2.0 * SUN_DISC;
                        let dy = (rng.next() - 0.5) * 2.0 * SUN_DISC;
                        let direction = normalize([
                            sun[0] + t_axis[0] * dx + b_axis[0] * dy,
                            sun[1] + t_axis[1] * dx + b_axis[1] * dy,
                            sun[2] + t_axis[2] * dx + b_axis[2] * dy,
                        ]);
                        if !bvh.occluded(start, direction, 1.0e5) {
                            lit += 1.0 / SUN_SAMPLES as f32;
                        }
                    }
                }
            }
            // Ambient: cosine-weighted hemisphere rays with a reach; what
            // escapes counts as sky, what hits nearby counts as occluded,
            // with nearer hits darker.
            let mut open = 0.0f32;
            for _ in 0..AMBIENT_SAMPLES {
                let r1 = rng.next();
                let r2 = rng.next();
                let radius = r1.sqrt();
                let angle = r2 * std::f32::consts::TAU;
                let (sx, sy) = (radius * angle.cos(), radius * angle.sin());
                let sz = (1.0 - r1).max(0.0).sqrt();
                let direction = normalize([
                    t_axis[0] * sx + b_axis[0] * sy + normal[0] * sz,
                    t_axis[1] * sx + b_axis[1] * sy + normal[1] * sz,
                    t_axis[2] * sx + b_axis[2] * sy + normal[2] * sz,
                ]);
                if !bvh.occluded(start, direction, AMBIENT_REACH) {
                    open += 1.0;
                }
            }
            // Stored as the open fraction of the sky; the engine decides how
            // much bounce an occluded corner keeps.
            let open = open / AMBIENT_SAMPLES as f32;
            let at = (py * w + px) as usize;
            sun_texels[at] = (lit * 255.0).round() as u8;
            ambient_texels[at] = (open * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    (sun_texels, ambient_texels)
}

impl Bake {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.texel.to_le_bytes());
        out.extend_from_slice(&self.page_size.to_le_bytes());
        out.extend_from_slice(&(self.pages.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.charts.len() as u32).to_le_bytes());
        for chart in &self.charts {
            match chart {
                None => out.push(0),
                Some(c) => {
                    out.push(1);
                    out.extend_from_slice(&c.page.to_le_bytes());
                    out.extend_from_slice(&c.x.to_le_bytes());
                    out.extend_from_slice(&c.y.to_le_bytes());
                    out.extend_from_slice(&c.width.to_le_bytes());
                    out.extend_from_slice(&c.height.to_le_bytes());
                    for v in [c.origin, c.u_axis, c.v_axis] {
                        for x in v {
                            out.extend_from_slice(&x.to_le_bytes());
                        }
                    }
                }
            }
        }
        out.extend_from_slice(&(self.face_sun.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.face_sun);
        out.extend_from_slice(&(self.vertex_open.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.vertex_open);
        let mut texels = Vec::with_capacity(self.pages.len() * 2 * (self.page_size as usize).pow(2));
        for page in &self.pages {
            texels.extend_from_slice(&page.sun);
            texels.extend_from_slice(&page.ambient);
        }
        // Sun masks are long runs of lit and shadow; both pages pack down
        // several times over.
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        use std::io::Write as _;
        let _ = encoder.write_all(&texels);
        let packed = encoder.finish().unwrap_or(texels);
        out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        out.extend_from_slice(&packed);
        out
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() < 24 || &data[..8] != MAGIC {
            return Err("lightmap: not a bake file".into());
        }
        let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        let texel = f32::from_bits(u32_at(8));
        let page_size = u32_at(12);
        let page_count = u32_at(16) as usize;
        let chart_count = u32_at(20) as usize;
        let mut at = 24;
        let mut charts = Vec::with_capacity(chart_count);
        for _ in 0..chart_count {
            let tag = *data.get(at).ok_or("lightmap: charts truncated")?;
            at += 1;
            if tag == 0 {
                charts.push(None);
                continue;
            }
            if at + 10 + 36 > data.len() {
                return Err("lightmap: chart truncated".into());
            }
            let u16_at = |p: usize| u16::from_le_bytes([data[p], data[p + 1]]);
            let f32_at = |p: usize| f32::from_le_bytes(data[p..p + 4].try_into().unwrap());
            let vec_at = |p: usize| [f32_at(p), f32_at(p + 4), f32_at(p + 8)];
            charts.push(Some(Chart {
                page: u16_at(at),
                x: u16_at(at + 2),
                y: u16_at(at + 4),
                width: u16_at(at + 6),
                height: u16_at(at + 8),
                origin: vec_at(at + 10),
                u_axis: vec_at(at + 22),
                v_axis: vec_at(at + 34),
            }));
            at += 46;
        }
        let take = |at: &mut usize| -> Result<Vec<u8>, String> {
            if *at + 4 > data.len() {
                return Err("lightmap: prop tables truncated".into());
            }
            let n = u32_at(*at) as usize;
            *at += 4;
            let bytes = data.get(*at..*at + n).ok_or("lightmap: prop tables truncated")?.to_vec();
            *at += n;
            Ok(bytes)
        };
        let face_sun = take(&mut at)?;
        let vertex_open = take(&mut at)?;
        let texels = (page_size as usize) * (page_size as usize);
        if at + 4 > data.len() {
            return Err("lightmap: pages truncated".into());
        }
        let packed_len = u32_at(at) as usize;
        at += 4;
        let packed = data.get(at..at + packed_len).ok_or("lightmap: pages truncated")?;
        let mut unpacked = Vec::with_capacity(page_count * 2 * texels);
        {
            use std::io::Read as _;
            flate2::read::ZlibDecoder::new(packed)
                .read_to_end(&mut unpacked)
                .map_err(|error| format!("lightmap: pages: {error}"))?;
        }
        if unpacked.len() < page_count * 2 * texels {
            return Err("lightmap: pages truncated".into());
        }
        let mut pages = Vec::with_capacity(page_count);
        for index in 0..page_count {
            let start = index * 2 * texels;
            pages.push(Page {
                sun: unpacked[start..start + texels].to_vec(),
                ambient: unpacked[start + texels..start + 2 * texels].to_vec(),
            });
        }
        Ok(Self {
            texel,
            page_size,
            pages,
            charts,
            face_sun,
            vertex_open,
        })
    }
}
