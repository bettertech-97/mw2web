//! Finds floor a player can see but not stand on in an imported Source map:
//! for every visible upward-facing surface on a grid, a small box swept down
//! through the map's own clip collision (the tables the game moves players
//! against) must stop at that floor. Spots where it does not are holes.
//!
//!   source_holes <map.vmf> [step=16] [--props]
//!
//! `--props` also checks prop tops (props have no collision yet, so every
//! prop top is a known hole; off by default to keep the report on the map).

use std::collections::BTreeMap;

use asset_world::world_source::build_source_clip_collision;
use asset_world::MASK_PLAYER_SOLID;

struct Tri {
    p: [[f32; 3]; 3],
    surface: usize,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: source_holes <map.vmf> [step] [--props]");
    let rest: Vec<String> = args.collect();
    let step: f32 = rest.iter().find_map(|a| a.parse().ok()).unwrap_or(16.0);
    let with_props = rest.iter().any(|a| a == "--props");

    let text = std::fs::read_to_string(&path).expect("read vmf");
    let mut map = source_map::compile(&text, 256.0).expect("compile");
    let brush_surfaces = map.surfaces.len();
    if with_props {
        let props = std::path::Path::new(&path).with_extension("props.bin");
        if let Ok(bytes) = std::fs::read(&props) {
            let set = source_map::props::PropSet::from_bytes(&bytes).expect("props");
            source_map::props::append_props(&mut map, &set);
        }
    }
    let clip = build_source_clip_collision(&map);
    eprintln!(
        "{}: {} surfaces ({} from props), {} brushes, {} mesh tris, bounds {:?}..{:?}",
        path,
        map.surfaces.len(),
        map.surfaces.len() - brush_surfaces,
        clip.brushes.len(),
        clip.mesh.tri_indices.len() / 3,
        map.mins,
        map.maxs
    );

    // Upward-facing visible triangles, binned on a 2D grid.
    const BIN: f32 = 128.0;
    let mut bins: BTreeMap<(i32, i32), Vec<usize>> = BTreeMap::new();
    let mut tris: Vec<Tri> = Vec::new();
    let mut steep: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (si, surface) in map.surfaces.iter().enumerate() {
        // Visible floors only: sky, tool faces and downward faces (the
        // underside of a displacement's brush) are not stood on.
        if surface.sky || surface.material.starts_with("tools/") || surface.normal[2] < 0.3 {
            continue;
        }
        let first = surface.first_index as usize;
        let indices = &map.indices[first..first + surface.index_count as usize];
        for tri in indices.chunks_exact(3) {
            let p = [0, 1, 2].map(|k| {
                map.vertices[tri[k] as usize].position
            });
            let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
            let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
            let n = [
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            ];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len < 1e-6 || (n[2] / len).abs() < 0.05 {
                continue;
            }
            // Slopes too steep to stand on still count as seen (`--floating`).
            let index = tris.len();
            if (n[2] / len).abs() < 0.7 {
                steep.insert(index);
            }
            tris.push(Tri { p, surface: si });
            let (x0, x1) = (p.iter().map(|v| v[0]).fold(f32::MAX, f32::min), p.iter().map(|v| v[0]).fold(f32::MIN, f32::max));
            let (y0, y1) = (p.iter().map(|v| v[1]).fold(f32::MAX, f32::min), p.iter().map(|v| v[1]).fold(f32::MIN, f32::max));
            for bx in (x0 / BIN).floor() as i32..=(x1 / BIN).floor() as i32 {
                for by in (y0 / BIN).floor() as i32..=(y1 / BIN).floor() as i32 {
                    bins.entry((bx, by)).or_default().push(index);
                }
            }
        }
    }

    // Height of the triangle's plane at (x, y) when the point is inside it.
    let height_at = |t: &Tri, x: f32, y: f32| -> Option<f32> {
        let [a, b, c] = t.p;
        let d = (b[1] - c[1]) * (a[0] - c[0]) + (c[0] - b[0]) * (a[1] - c[1]);
        if d.abs() < 1e-9 {
            return None;
        }
        let w0 = ((b[1] - c[1]) * (x - c[0]) + (c[0] - b[0]) * (y - c[1])) / d;
        let w1 = ((c[1] - a[1]) * (x - c[0]) + (a[0] - c[0]) * (y - c[1])) / d;
        let w2 = 1.0 - w0 - w1;
        let eps = -1e-4;
        (w0 >= eps && w1 >= eps && w2 >= eps).then(|| w0 * a[2] + w1 * b[2] + w2 * c[2])
    };

    // `--at x y`: explain one point instead of scanning.
    if let Some(i) = rest.iter().position(|a| a == "--at") {
        let x: f32 = rest[i + 1].parse().unwrap();
        let y: f32 = rest[i + 2].parse().unwrap();
        let list = bins.get(&((x / BIN).floor() as i32, (y / BIN).floor() as i32)).cloned().unwrap_or_default();
        for &ti in &list {
            if let Some(z) = height_at(&tris[ti], x, y) {
                let surface = &map.surfaces[tris[ti].surface];
                println!("render floor z={z:.1} surface#{} disp={} mat={} normal={:?}", tris[ti].surface, surface.displacement, surface.material, surface.normal);
            }
        }
        // Mesh triangles under the point.
        let mesh = &clip.mesh;
        for (t, tri) in mesh.tri_indices.chunks_exact(3).enumerate() {
            // Partition of this triangle -> vertex segment.
            let part = mesh.partitions.iter().find(|p| (p.first_tri as usize..p.first_tri as usize + p.tri_count as usize).contains(&t));
            let Some(part) = part else { continue };
            let base = part.first_vert_segment as usize * clipmap_iw4::VERTS_PER_SEGMENT;
            let p = [0, 1, 2].map(|k| mesh.verts[base + tri[k] as usize]);
            let fake = Tri { p, surface: 0 };
            if let Some(z) = height_at(&fake, x, y) {
                let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
                let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
                let nz = e1[0] * e2[1] - e1[1] * e2[0];
                println!("mesh tri {t} z={z:.1} normal_z_sign={} verts={p:?}", nz.signum());
            }
        }
        // The partition and tree leaf of every mesh triangle under the point,
        // and a mesh-only trace.
        for (t, tri) in mesh.tri_indices.chunks_exact(3).enumerate() {
            let Some((pi, part)) = mesh.partitions.iter().enumerate().find(|(_, p)| (p.first_tri as usize..p.first_tri as usize + p.tri_count as usize).contains(&t)) else { continue };
            let base = part.first_vert_segment as usize * clipmap_iw4::VERTS_PER_SEGMENT;
            let pts = [0, 1, 2].map(|k| mesh.verts[base + tri[k] as usize]);
            if height_at(&Tri { p: pts, surface: 0 }, x, y).is_none() { continue; }
            let leaf = mesh.aabb_trees.iter().position(|n| n.child_count == 0 && n.u == pi as i32);
            println!("tri {t}: partition {pi} (tris {}..+{}, segment {}), tree leaf {:?}", part.first_tri, part.tri_count, part.first_vert_segment, leaf);
            if let Some(li) = leaf {
                let n = &mesh.aabb_trees[li];
                println!("  leaf bounds origin={:?} half={:?}", n.origin, n.half_size);
                // Path from the root: is the leaf reachable?
                let mut parent = vec![usize::MAX; mesh.aabb_trees.len()];
                for (i, node) in mesh.aabb_trees.iter().enumerate() {
                    if node.child_count > 0 && node.u >= 0 {
                        for c in 0..node.child_count as usize { parent[node.u as usize + c] = i; }
                    }
                }
                let mut at = li; let mut depth = 0;
                while parent[at] != usize::MAX { at = parent[at]; depth += 1; }
                println!("  reaches root {at} at depth {depth}; roots={:?}", mesh.aabb_roots);
            }
        }
        {
            let floor = list.iter().filter_map(|&ti| height_at(&tris[ti], x, y)).fold(f32::MIN, f32::max);
            let ext = clipmap_iw4::TraceExtents::new([x, y, floor + 24.0], [x, y, floor - 200.0], [-2.0; 3], [2.0; 3], MASK_PLAYER_SOLID);
            let mesh_ref = clip.mesh.as_ref().as_ref();
            let t = clipmap_iw4::trace_through_mesh(&mesh_ref, &ext);
            println!("mesh-only trace: fraction={:.3} normal={:?}", t.fraction, t.normal);
        }
        for dz in [24.0f32, 8.0, 2.0] {
            let floor = list.iter().filter_map(|&ti| height_at(&tris[ti], x, y)).fold(f32::MIN, f32::max);
            let start = [x, y, floor + dz];
            for (label, ext) in [("point", 0.0f32), ("box2", 2.0), ("box15", 15.0)] {
                let hit = clip.sweep_box(start, [x, y, floor - 200.0], [-ext, -ext, 0.0], [ext, ext, ext.max(1.0)], MASK_PLAYER_SOLID);
                let stop = start[2] + (floor - 200.0 - start[2]) * hit.fraction;
                println!("from +{dz} {label}: fraction={:.3} stop_z={stop:.1} startsolid={} normal={:?}", hit.fraction, hit.startsolid, hit.normal);
            }
        }
        return;
    }

    // `--floating`: the other way round, solid nobody can see standing over
    // a visible floor, where a player walks in midair. A box dropped from
    // head height over each visible floor must reach it; where it stops more
    // than 12 units above, the brush holding it up is reported (contents and
    // bounds, to find it in the .vmf).
    if rest.iter().any(|a| a == "--floating") {
        let inside = |b: &source_map::CollisionBrush, p: [f32; 3]| {
            b.planes.iter().all(|pl| pl[0] * p[0] + pl[1] * p[1] + pl[2] * p[2] - pl[3] <= 0.01)
        };
        let mut found: BTreeMap<(i32, i32, i32), (usize, [f32; 4], String)> = BTreeMap::new();
        let mut checked = 0usize;
        let (mut x, x_end) = (map.mins[0].ceil(), map.maxs[0]);
        while x <= x_end {
            let mut y = map.mins[1].ceil();
            while y <= map.maxs[1] {
                if let Some(list) = bins.get(&((x / BIN).floor() as i32, (y / BIN).floor() as i32)) {
                    let seen: Vec<f32> = list.iter().filter_map(|&i| height_at(&tris[i], x, y)).collect();
                    let mut floors: Vec<f32> = list
                        .iter()
                        .filter(|&&i| !steep.contains(&i))
                        .filter_map(|&i| height_at(&tris[i], x, y))
                        .collect();
                    floors.sort_by(|a, b| b.total_cmp(a));
                    floors.dedup_by(|a, b| (*a - *b).abs() < 2.0);
                    for &z in &floors {
                        let top = [x, y, z + 64.0];
                        let room = clip.sweep_box([x, y, z + 72.0], top, [-2.0; 3], [2.0; 3], MASK_PLAYER_SOLID);
                        if room.startsolid || room.fraction < 1.0 {
                            continue;
                        }
                        checked += 1;
                        let hit = clip.sweep_box(top, [x, y, z], [-2.0; 3], [2.0; 3], MASK_PLAYER_SOLID);
                        if hit.startsolid || hit.fraction >= 1.0 {
                            continue;
                        }
                        let stop = top[2] + (z - top[2]) * hit.fraction;
                        // Stopped on another visible floor above this one
                        // (a step, a wall top): seen, so not midair.
                        if stop <= z + 12.0 || seen.iter().any(|f| (f - stop).abs() <= 3.0) {
                            continue;
                        }
                        let probe = [x, y, stop - 1.0];
                        let what = map
                            .brushes
                            .iter()
                            .find(|b| inside(b, probe))
                            .map(|b| {
                                format!(
                                    "contents {:#x} {} bounds {:?}..{:?}",
                                    b.contents,
                                    b.surface_type,
                                    b.mins.map(|v| v.round()),
                                    b.maxs.map(|v| v.round())
                                )
                            })
                            .unwrap_or_else(|| "a mesh (displacement)".to_owned());
                        let key = ((x / 128.0).floor() as i32, (y / 128.0).floor() as i32, (z / 128.0).floor() as i32);
                        let entry = found.entry(key).or_insert((0, [x, y, stop - z, z], what));
                        entry.0 += 1;
                    }
                }
                y += step;
            }
            x += step;
        }
        let mut rows: Vec<_> = found.into_values().collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        println!("floors checked: {checked}, floating spots: {} clusters", rows.len());
        for (n, p, what) in rows.iter().take(40) {
            println!("{n:5} near ({:.0} {:.0} {:.0}) {:.0} units up: {what}", p[0], p[1], p[3], p[2]);
        }
        return;
    }

    let mut checked = 0usize;
    let mut holes: Vec<([f32; 3], usize)> = Vec::new();
    let (mut x, x_end) = (map.mins[0].ceil(), map.maxs[0]);
    while x <= x_end {
        let mut y = map.mins[1].ceil();
        while y <= map.maxs[1] {
            if let Some(list) = bins.get(&((x / BIN).floor() as i32, (y / BIN).floor() as i32)) {
                let mut floors: Vec<(f32, usize)> = list
                    .iter()
                    .filter(|&&i| !steep.contains(&i))
                    .filter_map(|&i| height_at(&tris[i], x, y).map(|z| (z, tris[i].surface)))
                    .collect();
                floors.sort_by(|a, b| b.0.total_cmp(&a.0));
                floors.dedup_by(|a, b| (a.0 - b.0).abs() < 2.0);
                for (z, surface) in floors {
                    // A floor with something solid just above it (inside a
                    // wall, under a stair step) is not somewhere to stand.
                    let start = [x, y, z + 24.0];
                    let lid = clip.sweep_box([x, y, z + 72.0], start, [-2.0; 3], [2.0; 3], MASK_PLAYER_SOLID);
                    if lid.startsolid || lid.fraction < 1.0 {
                        continue;
                    }
                    checked += 1;
                    let hit = clip.sweep_box(start, [x, y, z - 40.0], [-2.0; 3], [2.0; 3], MASK_PLAYER_SOLID);
                    if hit.startsolid {
                        continue;
                    }
                    let stop = start[2] + (z - 40.0 - start[2]) * hit.fraction;
                    if hit.fraction >= 1.0 || stop < z - 12.0 {
                        holes.push(([x, y, z], surface));
                    }
                }
            }
            y += step;
        }
        x += step;
    }

    // Clusters on a 128-unit grid, largest first.
    let mut clusters: BTreeMap<(i32, i32, i32), (usize, [f32; 3], BTreeMap<String, usize>)> = BTreeMap::new();
    for (p, surface) in &holes {
        let key = ((p[0] / 128.0).floor() as i32, (p[1] / 128.0).floor() as i32, (p[2] / 128.0).floor() as i32);
        let entry = clusters.entry(key).or_insert((0, *p, BTreeMap::new()));
        entry.0 += 1;
        *entry.2.entry(map.surfaces[*surface].material.clone()).or_default() += 1;
    }
    let mut rows: Vec<_> = clusters.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!("standable floor points checked: {checked}, holes: {} in {} clusters", holes.len(), rows.len());
    for (_, (n, p, mats)) in rows.iter().take(40) {
        let mut mats: Vec<_> = mats.iter().collect();
        mats.sort_by(|a, b| b.1.cmp(a.1));
        let mats: Vec<String> = mats.iter().take(3).map(|(m, c)| format!("{m}x{c}")).collect();
        println!("{n:5} holes near ({:.0} {:.0} {:.0})  {}", p[0], p[1], p[2], mats.join(", "));
    }
}
