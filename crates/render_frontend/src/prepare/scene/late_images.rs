//! Textures a match deferred, decoded when something first draws them.
//!
//! The common zone's weapon textures are decoded with the map only for the
//! weapons a player can hold at spawn (`assets::fpv_defer`); the rest keep a
//! `deferred_request` on their catalog row. The first-person job and the
//! remote kits ask for a missing image through [`LateImageRequests`]; this
//! system decodes the batch on the load pool and republishes the live handle
//! table, which the GPU registry and both askers already watch for a new
//! `Arc`.

use std::collections::HashSet;
use std::sync::Arc;

use bevy::prelude::*;
use bevy::tasks::futures_lite::future;
use render_scene::{LateImageRequests, TessMaterials};

use super::world::WorldScene;

type Decoded = Vec<(usize, Arc<Image>)>;

#[derive(Resource, Default)]
pub struct LateImages {
    /// The decode on the pool, with what it was asked for and when.
    inflight: Option<(bevy::tasks::Task<Decoded>, usize, web_time::Instant)>,
    queue: Vec<u32>,
    /// Everything asked for under the current material definitions, so a
    /// slot is decoded once however many askers name it.
    requested: HashSet<u32>,
    owner: Option<*const asset_material::MaterialDefinitions>,
    decoded: usize,
}

// The raw pointer only identifies the definitions; nothing is read through it.
unsafe impl Send for LateImages {}
unsafe impl Sync for LateImages {}

pub(crate) fn decode_late_images(
    mut late: ResMut<LateImages>,
    requests: Res<LateImageRequests>,
    scene: Option<Res<WorldScene>>,
    mut tess: ResMut<TessMaterials>,
    mut gpu_handles: Option<ResMut<crate::assemble::drawsurf::RuntimeImageHandles>>,
    mut images: ResMut<Assets<Image>>,
) {
    let Some(definitions) = scene
        .as_ref()
        .and_then(|scene| scene.material_definitions.clone())
    else {
        requests.take();
        return;
    };
    let owner = Arc::as_ptr(&definitions);
    if late.owner != Some(owner) {
        late.owner = Some(owner);
        late.requested.clear();
        late.queue.clear();
        late.inflight = None;
        late.decoded = 0;
    }
    for image in requests.take() {
        let slot = image as usize;
        let missing = tess
            .material_images
            .get(slot)
            .is_some_and(Option::is_none);
        let row = definitions.images.get(slot);
        let deferred = row.is_some_and(|row| row.deferred_request.is_some());
        if late.requested.insert(image) {
            diag::debug!(
                World,
                "late image asked: slot {slot} `{}` handle_missing={missing} deferred={deferred} decoded={} payload={}B",
                row.map_or("?", |row| row.name.as_str()),
                row.is_some_and(|row| row.decoded.is_some()),
                row.map_or(0, |row| row.payload.len())
            );
            if missing && deferred {
                late.queue.push(image);
            }
        }
    }
    if let Some((task, asked, started)) = late.inflight.as_mut()
        && let Some(done) = future::block_on(future::poll_once(task))
    {
        let asked = *asked;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        late.inflight = None;
        if !done.is_empty() {
            let mut table = (*tess.material_images).clone();
            let mut names = Vec::new();
            let mut landed_handles = Vec::with_capacity(done.len());
            for (index, image) in done {
                if index >= table.len() {
                    continue;
                }
                let image = Arc::try_unwrap(image).unwrap_or_else(|shared| (*shared).clone());
                let handle = images.add(image);
                table[index] = Some(handle.clone());
                landed_handles.push((index, handle));
                if names.len() < 6 {
                    if let Some(row) = definitions.images.get(index) {
                        names.push(row.name.to_string());
                    }
                }
            }
            let landed = table.iter().flatten().count();
            tess.material_images = Arc::new(table);
            // The GPU registry reads the drawsurf handle set, not the tess
            // table, and rescans its slots only when that set is a new `Arc`:
            // an in-place edit would leave the late slots unseen.
            if let Some(gpu_handles) = gpu_handles.as_mut() {
                let mut material_images = gpu_handles.material_images.clone();
                for (index, handle) in &landed_handles {
                    if let Some(slot) = material_images.get_mut(*index) {
                        *slot = Some(handle.clone());
                    }
                }
                **gpu_handles = crate::assemble::drawsurf::RuntimeImageHandles::from_pools(
                    gpu_handles.generation_id,
                    material_images,
                    gpu_handles.material_names.clone(),
                    gpu_handles.reflection_probes.clone(),
                    gpu_handles.lightmaps.clone(),
                    gpu_handles.model_lighting.clone(),
                );
            }
            late.decoded += asked;
            diag::info!(
                World,
                "late images: {asked} decoded in {elapsed_ms:.0}ms ({} so far; {landed} slots live): {}",
                late.decoded,
                names.join(", ")
            );
        } else {
            diag::warn!(World, "late images: a batch of {asked} produced nothing");
        }
    }
    if late.inflight.is_none() && !late.queue.is_empty() {
        let indices: Vec<usize> = late.queue.drain(..).map(|image| image as usize).collect();
        let asked = indices.len();
        let definitions = Arc::clone(&definitions);
        let task = assets::load_pool().spawn(async move {
            asset_material::material_images::decode_deferred_images(
                &definitions,
                &indices,
                assets::load_pool(),
            )
        });
        late.inflight = Some((task, asked, web_time::Instant::now()));
    }
}
