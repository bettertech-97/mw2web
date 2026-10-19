use std::sync::Arc;

use bevy::prelude::*;
use render_material::{PreparedMaterialTable, RuntimeMaterialCatalog};

/// Catalog images the first-person job and the remote kits found missing
/// from the live handle table: a match deferred them, and the late-image
/// decoder (render_frontend) decodes them and republishes the table.
#[derive(Resource, Default)]
pub struct LateImageRequests {
    wanted: std::sync::Mutex<Vec<u32>>,
}

impl LateImageRequests {
    pub fn request(&self, image: u32) {
        let mut wanted = self.wanted.lock().unwrap_or_else(|e| e.into_inner());
        if !wanted.contains(&image) {
            wanted.push(image);
        }
    }

    pub fn take(&self) -> Vec<u32> {
        std::mem::take(&mut *self.wanted.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[derive(Resource, Clone, Debug)]
pub struct TessMaterials {
    catalog: Arc<RuntimeMaterialCatalog>,
    prepared: Arc<PreparedMaterialTable>,

    pub material_images: Arc<Vec<Option<Handle<Image>>>>,
    /// Per catalog image: whether the match deferred its decode to first
    /// use, so an admission can wait for it instead of refusing the surface.
    pub deferred_images: Arc<Vec<bool>>,
}

impl TessMaterials {
    pub fn new(
        catalog: Arc<RuntimeMaterialCatalog>,
        prepared: Arc<PreparedMaterialTable>,
    ) -> Result<Self, render_material::MaterialRefusal> {
        if catalog.generation_id() != prepared.generation_id() {
            return Err(render_material::MaterialRefusal::StaleMaterialGeneration {
                retained: prepared.generation_id(),
                current: catalog.generation_id(),
            });
        }
        Ok(Self {
            catalog,
            prepared,
            material_images: Arc::new(Vec::new()),
            deferred_images: Arc::new(Vec::new()),
        })
    }
    pub fn catalog(&self) -> &Arc<RuntimeMaterialCatalog> {
        &self.catalog
    }
    pub fn prepared(&self) -> &Arc<PreparedMaterialTable> {
        &self.prepared
    }
}

impl Default for TessMaterials {
    fn default() -> Self {
        let catalog = Arc::new(RuntimeMaterialCatalog::default());
        let prepared = Arc::new(PreparedMaterialTable::from_catalog(&catalog, |_, _| None));
        Self::new(catalog, prepared).expect("empty material publication")
    }
}

#[derive(Resource, Clone, Debug, Default)]
pub struct WorldPresentFacts {
    pub spawned: bool,
}
