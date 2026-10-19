//! The CS2 material that stands in for one a map names but CS2 lacks.
//!
//! CS2 rebuilt several maps (Inferno, Train, Overpass, Cache) and renamed
//! their surfaces: `plaster/hr_p/inferno/plaster_b_blend` became
//! `de_inferno/plaster/…`, many world textures moved under `models/`. The
//! same file name elsewhere is taken first; otherwise the material sharing
//! the most name words, of the same kind of surface (plaster, brick,
//! wood…), preferring the map's own folder.

use std::collections::{HashMap, HashSet};

/// Folders that hold no world surfaces.
const NOT_WORLD: &[&str] = &[
    "effects/", "decals/", "overlay", "tools/", "dev/", "sprites/", "particle", "vgui/", "glow",
    "skybox", "weapons/", "characters/", "models/player", "models/weapons", "models/inventory",
    "hud", "editor", "panorama/", "stickers/", "default/", "debug", "lights/",
];

/// Words that say nothing about what a surface looks like.
const NOISE: &[&str] = &["materials", "hr", "models", "props", "vmat", "color", "diffuse", "master"];

/// Words many unrelated surfaces share: a weak sign of likeness.
const GENERIC: &[&str] = &[
    "blend", "cheap", "wall", "ext", "int", "floor", "trim", "dirty", "clean", "old", "new", "dark",
    "light", "base", "top", "painted", "worn",
];

/// What a material looks like, for matching: the words of its path.
fn words(path: &str) -> Vec<String> {
    path.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| word.len() >= 3 && !NOISE.contains(word))
        .map(str::to_owned)
        .collect()
}

/// The file name without the suffixes that only say how a surface is used.
fn base(path: &str) -> &str {
    let mut name = path.rsplit('/').next().unwrap_or(path).trim_end_matches('_');
    for suffix in ["_blend", "_cheap", "_nobump", "_noblend", "_dx80"] {
        name = name.strip_suffix(suffix).unwrap_or(name);
    }
    name
}

pub struct Nearest {
    /// Material paths (`plaster/x`, no `materials/` or extension).
    paths: Vec<String>,
    words: Vec<Vec<String>>,
    by_base: HashMap<String, Vec<usize>>,
    /// The map's short name (`inferno` for `de_inferno`), favoured.
    map: String,
}

impl Nearest {
    pub fn new<'a>(entries: impl Iterator<Item = &'a str>, map: &str) -> Self {
        let mut paths: Vec<String> = entries
            .filter_map(|name| name.strip_prefix("materials/")?.strip_suffix(".vmat_c"))
            .filter(|path| !NOT_WORLD.iter().any(|skip| path.contains(skip)))
            .filter(|path| !words(path).iter().any(|word| word == "test"))
            .map(str::to_owned)
            .collect();
        paths.sort();
        let mut by_base: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, path) in paths.iter().enumerate() {
            by_base.entry(base(path).to_owned()).or_default().push(index);
        }
        let map = map
            .trim_start_matches("de_")
            .trim_start_matches("cs_")
            .trim_start_matches("mp_")
            .to_owned();
        Self {
            words: paths.iter().map(|path| words(path)).collect(),
            paths,
            by_base,
            map,
        }
    }

    /// The best stand-in for `material` (`plaster/x`), if any looks close
    /// enough.
    #[cfg(test)]
    pub fn find(&self, material: &str) -> Option<&str> {
        self.ranked(material, 1).into_iter().next()
    }

    /// Up to `limit` stand-ins for `material`, best first (never the
    /// material itself): the same file name elsewhere, a world folder before
    /// models, then materials of the same kind sharing the most name words.
    /// The caller tries them in turn: some CS2 materials name a colour
    /// texture CS2 does not ship.
    pub fn ranked(&self, material: &str, limit: usize) -> Vec<&str> {
        let mut order: Vec<usize> = Vec::new();
        if let Some(found) = self.by_base.get(base(material)) {
            let mut found = found.clone();
            found.sort_by_key(|&index| (self.paths[index].starts_with("models/"), index));
            order.extend(found);
        }
        // The kind of surface: the first word of the first folder that is
        // not a map's own (`plaster` in `de_inferno/plaster/x`, `stone` in
        // `de_cbble/stone_alt/x`), else the file's first word.
        let folders: Vec<&str> = material.split('/').collect();
        let kind = folders[..folders.len() - 1]
            .iter()
            .filter(|folder| !["de_", "cs_", "ar_", "gd_"].iter().any(|map| folder.starts_with(map)))
            .find_map(|folder| words(folder).into_iter().next())
            .or_else(|| words(base(material)).into_iter().next());
        if let Some(kind) = kind {
            let wanted = words(material);
            let mut scored: Vec<(i32, usize)> = Vec::new();
            for (index, have) in self.words.iter().enumerate() {
                if !have.iter().any(|word| word.contains(kind.as_str())) {
                    continue;
                }
                let shared: i32 = wanted
                    .iter()
                    .filter(|word| **word != kind && have.contains(word))
                    .map(|word| if GENERIC.contains(&word.as_str()) { 3 } else { 10 })
                    .sum();
                let path = &self.paths[index];
                let ours = path.contains(self.map.as_str());
                // Another map's own surface fits worse than a shared one.
                let theirs = !ours
                    && path
                        .split('/')
                        .any(|folder| ["de_", "cs_", "ar_", "gd_"].iter().any(|map| folder.starts_with(map)));
                // The file named for the kind (`inferno_plaster_01`), more so
                // when that is its first word after the map's name.
                let name_words: Vec<String> =
                    words(base(path)).into_iter().filter(|word| *word != self.map).collect();
                let named = name_words.iter().any(|word| word.contains(kind.as_str()));
                let leads = name_words.first().is_some_and(|word| word.contains(kind.as_str()));
                let score = shared
                    + if ours { 8 } else { 0 }
                    - if theirs { 4 } else { 0 }
                    + if named { 4 } else { 0 }
                    + if leads { 6 } else { 0 }
                    - if path.starts_with("models/") { 3 } else { 0 };
                scored.push((score, index));
            }
            scored.sort_by_key(|&(score, index)| (std::cmp::Reverse(score), index));
            // Equally close stand-ins are spread by the material's name, so
            // `plaster_a` and `plaster_b` keep looking like two surfaces.
            let tied = scored.iter().take_while(|(score, _)| Some(*score) == scored.first().map(|s| s.0)).count();
            if tied > 1 {
                let spread = material.bytes().fold(0usize, |h, b| h.wrapping_mul(31).wrapping_add(usize::from(b)));
                scored[..tied].rotate_left(spread % tied);
            }
            order.extend(scored.into_iter().map(|(_, index)| index));
        }
        let itself = [material.to_owned(), format!("{material}_")];
        let mut seen = HashSet::new();
        order
            .into_iter()
            .filter(|&index| seen.insert(index) && !itself.contains(&self.paths[index]))
            .take(limit)
            .map(|index| self.paths[index].as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Nearest;

    #[test]
    fn same_name_then_same_kind_from_the_map() {
        let entries = [
            "materials/de_inferno/plaster/inferno_plaster_01.vmat_c",
            "materials/de_anubis/hr_plaster_wall01.vmat_c",
            "materials/models/props/de_inferno/hr_i/wall_trim_b/brick_g1.vmat_c",
            "materials/brick/brick_floor_02.vmat_c",
            "materials/decals/plaster_crack.vmat_c",
        ];
        let nearest = Nearest::new(entries.into_iter(), "de_inferno");
        assert_eq!(
            nearest.find("brick/hr_brick/inferno/brick_g1"),
            Some("models/props/de_inferno/hr_i/wall_trim_b/brick_g1")
        );
        assert_eq!(
            nearest.find("plaster/hr_p/inferno/plaster_b_blend"),
            Some("de_inferno/plaster/inferno_plaster_01")
        );
        assert_eq!(nearest.find("tile/ceilingtilea"), None);
        // A material CS2 has is never its own stand-in.
        assert_eq!(
            nearest.ranked("de_inferno/plaster/inferno_plaster_01", 1),
            vec!["de_anubis/hr_plaster_wall01"]
        );
    }
}
