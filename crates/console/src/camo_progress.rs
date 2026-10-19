//! Earned camouflage, as MW2 did it: a weapon's camouflages unlock with
//! headshots scored with that weapon.
//!
//! This client counts its own headshot kills from the obituaries it is
//! shown and keeps the tally in `profile/camos.txt`, next to the classes,
//! so the page and the server carry it with the rest of the profile. The
//! class editor is the only reader: a camouflage still locked cannot be put
//! on a class. Counting on the client keeps the host and the wire as they
//! are; between friends that is enough.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use bevy::prelude::*;
use web_time::Instant;

const FILE_HEADER: &str = "IW4L_CAMOS 1";
/// Writes are batched: a kill streak is one save, not five.
const SAVE_DELAY: Duration = Duration::from_secs(2);
const RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Resource, Default)]
pub struct CamoProgress {
    /// Headshots by weapon family, keyed like the class editor's weapons
    /// (`iw4:weapon/ak47_mp`), whatever attachments the kill was made with.
    headshots: BTreeMap<String, u32>,
    path: Option<PathBuf>,
    dirty: bool,
    changed_at: Option<Instant>,
    failed_at: Option<Instant>,
}

impl CamoProgress {
    /// Headshots with the weapon a class names (`iw4:weapon/ak47_mp`, or
    /// any spelling `FamilyKey::parse` takes).
    pub fn headshots_with(&self, weapon: &str) -> u32 {
        let key = asset_game::FamilyKey::parse(weapon).map_or_else(|| weapon.to_owned(), |family| family.asset_key());
        self.headshots.get(&key).copied().unwrap_or(0)
    }

    fn credit(&mut self, family: &asset_game::FamilyKey) -> u32 {
        let count = self.headshots.entry(family.asset_key()).or_insert(0);
        *count += 1;
        self.dirty = true;
        self.changed_at = Some(Instant::now());
        *count
    }

    fn encode(&self) -> String {
        let mut out = String::from(FILE_HEADER);
        out.push('\n');
        for (key, count) in &self.headshots {
            out.push_str(key);
            out.push('\t');
            out.push_str(&count.to_string());
            out.push('\n');
        }
        out
    }

    fn decode(text: &str) -> Option<BTreeMap<String, u32>> {
        let mut lines = text.lines();
        if lines.next()?.trim() != FILE_HEADER {
            return None;
        }
        let mut headshots = BTreeMap::new();
        for line in lines {
            let Some((key, count)) = line.split_once('\t') else {
                continue;
            };
            if let Ok(count) = count.trim().parse::<u32>()
                && !key.is_empty()
            {
                headshots.insert(key.to_owned(), count);
            }
        }
        Some(headshots)
    }
}

pub(crate) fn load(identity: Res<ui::LaunchIdentity>, mut progress: ResMut<CamoProgress>) {
    if identity.artifacts.as_os_str().is_empty() {
        return;
    }
    let path = identity.artifacts.join("profile").join("camos.txt");
    match vfs::read_to_string(&path) {
        Ok(text) => match CamoProgress::decode(&text) {
            Some(headshots) => {
                diag::info!(
                    Ui,
                    "camo: {} weapons with headshots read from {}",
                    headshots.len(),
                    path.display()
                );
                progress.headshots = headshots;
            }
            None => diag::warn!(Ui, "camo: {} is not a camo file this build reads", path.display()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => diag::warn!(Ui, "camo: could not read {}: {error}", path.display()),
    }
    progress.path = Some(path);
}

pub(crate) fn save(mut progress: ResMut<CamoProgress>) {
    if !progress.dirty {
        return;
    }
    let now = Instant::now();
    if progress
        .changed_at
        .is_some_and(|at| now.duration_since(at) < SAVE_DELAY)
        || progress
            .failed_at
            .is_some_and(|at| now.duration_since(at) < RETRY_DELAY)
    {
        return;
    }
    let Some(path) = progress.path.clone() else {
        return;
    };
    let written = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            vfs::create_dir_all(parent)?;
        }
        let pending = path.with_extension(format!("{}.tmp", vfs::process_id()));
        vfs::write(&pending, progress.encode())?;
        vfs::rename(&pending, &path)
    })();
    match written {
        Ok(()) => {
            progress.dirty = false;
            progress.failed_at = None;
        }
        Err(error) => {
            diag::warn!(Ui, "camo: could not save {}: {error}", path.display());
            progress.failed_at = Some(now);
        }
    }
}

/// Every obituary this client is shown: a headshot kill by the local
/// player counts for the weapon's family.
pub(crate) fn count_headshots(
    obituary: On<net::EntityObituary>,
    local: Res<net::LocalPresentClient>,
    catalog: Res<ui::ClassLoadoutCatalog>,
    mut progress: ResMut<CamoProgress>,
) {
    let shown = &*obituary;
    if shown.in_killcam {
        return;
    }
    let payload = &shown.event.payload;
    let me = local.0.0 as i32;
    if payload.attacker_entity_num != me || payload.number == me {
        return;
    }
    if hud_iw4::obituary_mod(payload.event_parm) != Some(hud_iw4::MOD_HEAD_SHOT) {
        return;
    }
    let Some(registry) = catalog.resolver.0.as_deref() else {
        return;
    };
    let Some(family) = registry
        .weapon_families()
        .describe(payload.weapon)
        .and_then(|selection| selection.family.clone())
    else {
        return;
    };
    let count = progress.credit(&family);
    diag::info!(Ui, "camo: {count} headshots with {}", family.asset_key());
}
