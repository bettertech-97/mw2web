use std::num::NonZeroU32;

use super::gpu_contract::{
    TEXTURE_TABLE_2D_CAPACITY, TEXTURE_TABLE_3D_CAPACITY, TEXTURE_TABLE_CUBE_CAPACITY,
    TEXTURE_TABLE_SAMPLER_CAPACITY,
};
use super::gpu_resources::{
    DecodedSampler, RuntimeUploadedImageRegistry, UploadedTextureBind, UploadedTextureIdentity,
};
use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use bevy::render::render_resource::{
    BindGroup, BindGroupEntry, BindGroupLayoutDescriptor, BindingResource, Extent3d, Sampler,
    SamplerDescriptor, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureView, TextureViewDescriptor, TextureViewDimension, TextureViewId, WgpuSampler,
    WgpuTextureView,
};
use bevy::render::renderer::RenderDevice;
use d3d9_sm3::{TEXTURE_TABLE_BINDING_2D, TEXTURE_TABLE_BINDING_3D, TEXTURE_TABLE_BINDING_CUBE};
use render_material::{MaterialGenerationId, SamplerTextureDimension};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SamplerKey {
    sampler_state: u8,
    packed_word: u32,
    anisotropy_clamp: u16,
    uses_mipmaps: bool,
}

impl From<&DecodedSampler> for SamplerKey {
    fn from(sampler: &DecodedSampler) -> Self {
        Self {
            sampler_state: sampler.sampler_state,
            packed_word: sampler.packed_word(),
            anisotropy_clamp: sampler.anisotropy_clamp(),
            uses_mipmaps: sampler.uses_mipmaps(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextureTableRefusal {
    TexturesFull {
        dimension: SamplerTextureDimension,
        capacity: u32,
    },
    SamplersFull {
        capacity: u32,
    },
}

pub(super) struct TextureTableBinds {
    pub scene: BindGroup,
    pub sun_caster: BindGroup,
}

struct Placeholders {
    views: [TextureView; 3],
    sampler: Sampler,
}

/// A table slot handed out on a worker thread, whose view is attached on the
/// device thread (`ExactTextureTable::attach_views`).
struct PendingView {
    lane: usize,
    index: u16,
    identity: UploadedTextureIdentity,
    dimension: SamplerTextureDimension,
    id: TextureViewId,
}

#[derive(Resource, Default, Deref, DerefMut)]
pub(super) struct ShadowTextureTable(pub ExactTextureTable);

#[derive(Resource, Default, Deref, DerefMut)]
pub(super) struct SceneTextureTables(pub [ExactTextureTable; 4]);

pub(super) fn scene_table_index(after_scene_resolve: bool, srgb_write: bool) -> usize {
    usize::from(after_scene_resolve) * 2 + usize::from(srgb_write)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TableEpoch {
    pub generation: MaterialGenerationId,

    pub replaced_revision: u64,
}

#[derive(Resource, Default)]
pub(super) struct ExactTextureTable {
    epoch: TableEpoch,
    /// `None` until a slot handed out on a worker thread has its view
    /// attached (`pending_views`).
    views: [Vec<Option<TextureView>>; 3],
    pending_views: Vec<PendingView>,
    view_index: HashMap<TextureViewId, u16>,

    sun_shadow_indices: Vec<u16>,
    samplers: Vec<Sampler>,
    /// Samplers given a slot but not created yet: slot words are handed out
    /// while draws are prepared, possibly on worker threads, where the
    /// browser's GPU device cannot be called; bind groups create them first.
    pending_samplers: Vec<(u16, DecodedSampler)>,
    sampler_index: HashMap<SamplerKey, u16>,
    placeholders: Option<Placeholders>,
    binds: Option<TextureTableBinds>,
    /// `binds` no longer matches the table; dropped by [`Self::binds`] on the
    /// device thread (dropping GPU handles on a worker thread is not allowed
    /// in the browser).
    binds_stale: bool,
    /// Browser build: one bind group per distinct slot-word list, without
    /// and with sun caster masking, built by [`Self::ensure_web_group`].
    /// Keyed by the list itself so a lookup borrows the draw's words.
    #[cfg(target_arch = "wasm32")]
    web_groups: [HashMap<Box<[u32]>, BindGroup>; 2],

    pub rebuild_n: u32,
}

const fn dimension_lane(dimension: SamplerTextureDimension) -> usize {
    match dimension {
        SamplerTextureDimension::D2 => 0,
        SamplerTextureDimension::Cube => 1,
        SamplerTextureDimension::D3 => 2,
    }
}

const fn capacity(dimension: SamplerTextureDimension) -> u32 {
    match dimension {
        SamplerTextureDimension::D2 => TEXTURE_TABLE_2D_CAPACITY,
        SamplerTextureDimension::Cube => TEXTURE_TABLE_CUBE_CAPACITY,
        SamplerTextureDimension::D3 => TEXTURE_TABLE_3D_CAPACITY,
    }
}

impl ExactTextureTable {
    pub fn epoch(&self) -> TableEpoch {
        self.epoch
    }

    pub fn open_epoch(&mut self, epoch: TableEpoch) {
        if self.epoch == epoch {
            return;
        }
        self.epoch = epoch;
        for lane in &mut self.views {
            lane.clear();
        }
        self.pending_views.clear();
        self.view_index.clear();
        self.binds_stale = false;
        self.sun_shadow_indices.clear();
        self.samplers.clear();
        self.pending_samplers.clear();
        self.sampler_index.clear();
        self.binds = None;
        #[cfg(target_arch = "wasm32")]
        for groups in &mut self.web_groups {
            groups.clear();
        }
    }

    pub fn slot_word(
        &mut self,
        device: &RenderDevice,
        lane: &UploadedTextureBind<'_>,
    ) -> Result<u32, TextureTableRefusal> {
        self.ensure_placeholders(device);
        let lane_i = dimension_lane(lane.dimension);
        let texture = match self.view_index.get(&lane.view.id()) {
            Some(&index) => index,
            None => {
                let cap = capacity(lane.dimension);
                let index = u32::try_from(self.views[lane_i].len()).expect("table index fits u32");
                if index >= cap {
                    return Err(TextureTableRefusal::TexturesFull {
                        dimension: lane.dimension,
                        capacity: cap,
                    });
                }
                let index = u16::try_from(index).expect("capacities fit the 16-bit slot half");
                // A worker thread may not clone the view (the browser's GPU
                // handles stay on the device thread): it is attached there.
                if web_threads::on_device_thread() {
                    self.views[lane_i].push(Some(lane.view.clone()));
                } else {
                    self.views[lane_i].push(None);
                    self.pending_views.push(PendingView {
                        lane: lane_i,
                        index,
                        identity: lane.identity,
                        dimension: lane.dimension,
                        id: lane.view.id(),
                    });
                }
                self.view_index.insert(lane.view.id(), index);
                if lane.identity == UploadedTextureIdentity::Code(super::CODE_TEXTURE_SHADOWMAP_SUN)
                {
                    self.sun_shadow_indices.push(index);
                }
                self.binds_stale = true;
                index
            }
        };
        let key = SamplerKey::from(&lane.sampler);
        let sampler = match self.sampler_index.get(&key) {
            Some(&index) => index,
            None => {
                let index = u32::try_from(self.samplers.len() + self.pending_samplers.len())
                    .expect("table index fits u32");
                if index >= TEXTURE_TABLE_SAMPLER_CAPACITY {
                    return Err(TextureTableRefusal::SamplersFull {
                        capacity: TEXTURE_TABLE_SAMPLER_CAPACITY,
                    });
                }
                let index = u16::try_from(index).expect("capacities fit the 16-bit slot half");
                // Created when a bind group needs it (`materialize_samplers`,
                // on the thread that owns the device); until then the slot
                // is only reserved.
                self.pending_samplers.push((index, lane.sampler));
                self.sampler_index.insert(key, index);
                self.binds_stale = true;
                index
            }
        };
        // The browser binds views per draw rather than indexing the table in
        // the shader, so its slot words also say which lane the index is in.
        #[cfg(target_arch = "wasm32")]
        let texture = texture | ((lane_i as u16) << WEB_LANE_SHIFT);
        Ok(d3d9_sm3::texture_slot_word(texture, sampler))
    }

    /// Browser build: the group-1 bind group for a draw whose pass ABI slots
    /// resolved to `slots` (words from [`Self::slot_word`]). Slot `i` binds its
    /// view at `2i` and sampler at `2i + 1`, matching the layout the pipeline
    /// derived from the same ABI. `mask_sun` stands the placeholder in for the
    /// sun shadow map, as the table's sun caster group does. Recording then
    /// finds it with [`Self::web_cached`].
    #[cfg(target_arch = "wasm32")]
    pub fn ensure_web_group(
        &mut self,
        device: &RenderDevice,
        registry: &super::exact_pipeline::ExactPipelineRegistry,
        slots: &[u32],
        mask_sun: bool,
    ) {
        self.ensure_placeholders(device);
        self.materialize_samplers(device);
        if self.web_groups[usize::from(mask_sun)].contains_key(slots) {
            return;
        }
        let lanes: Vec<(usize, usize, usize)> = slots
            .iter()
            .map(|&word| {
                let texture = (word & 0xffff) as usize;
                (
                    (texture >> WEB_LANE_SHIFT).min(2),
                    texture & ((1 << WEB_LANE_SHIFT) - 1),
                    (word >> 16) as usize,
                )
            })
            .collect();
        let dimensions = lanes.iter().map(|&(lane, _, _)| match lane {
            0 => SamplerTextureDimension::D2,
            1 => SamplerTextureDimension::Cube,
            _ => SamplerTextureDimension::D3,
        });
        let descriptor = super::colour_submit::bind_group_layout_from_entries(
            "iw4_web_textures",
            &render_frame::web_texture_bind_entries(dimensions),
            false,
        );
        let layout = registry.bind_group_layout(device, &descriptor);
        let placeholders = self.placeholders.as_ref().expect("built above");
        let mut entries = Vec::with_capacity(2 * lanes.len());
        for (slot, &(lane, index, sampler)) in lanes.iter().enumerate() {
            let masked = mask_sun && lane == 0 && self.sun_shadow_indices.contains(&(index as u16));
            let view = match self.views[lane].get(index) {
                Some(Some(view)) if !masked => view,
                _ => &placeholders.views[lane],
            };
            let sampler = self.samplers.get(sampler).unwrap_or(&placeholders.sampler);
            let binding = 2 * slot as u32;
            entries.push(BindGroupEntry {
                binding,
                resource: BindingResource::TextureView(&**view),
            });
            entries.push(BindGroupEntry {
                binding: binding + 1,
                resource: BindingResource::Sampler(&**sampler),
            });
        }
        let group = device.create_bind_group("iw4_web_textures", &layout, &entries);
        self.web_groups[usize::from(mask_sun)].insert(slots.into(), group);
    }

    /// Browser build: a group [`Self::ensure_web_group`] already built.
    #[cfg(target_arch = "wasm32")]
    pub fn web_cached(&self, slots: &[u32], mask_sun: bool) -> Option<&BindGroup> {
        self.web_groups[usize::from(mask_sun)].get(slots)
    }

    pub fn census(&self) -> (usize, usize, usize, usize) {
        let n = |lane: &Vec<Option<TextureView>>| lane.len().saturating_sub(1);
        (
            n(&self.views[0]),
            n(&self.views[1]),
            n(&self.views[2]),
            self.samplers.len().saturating_sub(1),
        )
    }

    pub fn binds(
        &mut self,
        device: &RenderDevice,
        registry: &super::exact_pipeline::ExactPipelineRegistry,
        layout: &BindGroupLayoutDescriptor,
    ) -> &TextureTableBinds {
        self.ensure_placeholders(device);
        self.materialize_samplers(device);
        if std::mem::take(&mut self.binds_stale) {
            self.binds = None;
        }
        if self.binds.is_none() {
            let bgl = registry.bind_group_layout(device, layout);
            let placeholder_2d = self
                .placeholders
                .as_ref()
                .expect("ensured above")
                .views[0]
                .clone();
            let scene = self.create_bind_group(device, &bgl, "iw4_texture_table_scene", None);
            let sun_caster = if self.sun_shadow_indices.is_empty() {
                scene.clone()
            } else {
                self.create_bind_group(
                    device,
                    &bgl,
                    "iw4_texture_table_sun_caster",
                    Some(&placeholder_2d),
                )
            };
            self.rebuild_n = self.rebuild_n.saturating_add(1);
            self.binds = Some(TextureTableBinds { scene, sun_caster });
        }
        self.binds.as_ref().expect("built above")
    }

    pub fn views_bind_group(
        &mut self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        label: &'static str,
        views: &[&TextureView],
        samplers: &[&Sampler],
        slots: &[u32],
    ) -> BindGroup {
        // Browser: the pass's slots bind directly, each word naming a view and
        // sampler of the lists given (`texture_slot_word(view, sampler)`).
        #[cfg(target_arch = "wasm32")]
        {
            let _ = self;
            let mut entries = Vec::with_capacity(2 * slots.len());
            for (slot, &word) in slots.iter().enumerate() {
                let binding = 2 * slot as u32;
                entries.push(BindGroupEntry {
                    binding,
                    resource: BindingResource::TextureView(&**views[(word & 0xffff) as usize]),
                });
                entries.push(BindGroupEntry {
                    binding: binding + 1,
                    resource: BindingResource::Sampler(&**samplers[(word >> 16) as usize]),
                });
            }
            return device.create_bind_group(label, bgl, &entries);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = slots;
        let views: Vec<&WgpuTextureView> = views.iter().map(|v| &***v).collect();
        let samplers: Vec<&WgpuSampler> = samplers.iter().map(|v| &***v).collect();
        self.ensure_placeholders(device);
        let placeholders = self.placeholders.as_ref().expect("built above");
        device.create_bind_group(
            label,
            bgl,
            &[
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_2D,
                    resource: BindingResource::TextureViewArray(&views),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_CUBE,
                    resource: BindingResource::TextureViewArray(&[&*placeholders.views[1]]),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_3D,
                    resource: BindingResource::TextureViewArray(&[&*placeholders.views[2]]),
                },
                BindGroupEntry {
                    binding: d3d9_sm3::TEXTURE_TABLE_BINDING_SAMPLERS,
                    resource: BindingResource::SamplerArray(&samplers),
                },
            ],
        )
    }

    fn create_bind_group(
        &self,
        device: &RenderDevice,
        bgl: &bevy::render::render_resource::BindGroupLayout,
        label: &'static str,
        mask_sun_shadow_with: Option<&TextureView>,
    ) -> BindGroup {
        let placeholders = self.placeholders.as_ref().expect("ensured by binds");
        let lane_views = |lane: usize| -> Vec<&WgpuTextureView> {
            self.views[lane]
                .iter()
                .map(|v| &**v.as_ref().unwrap_or(&placeholders.views[lane]))
                .collect()
        };
        let mut views_2d = lane_views(0);
        if let Some(placeholder) = mask_sun_shadow_with {
            for &index in &self.sun_shadow_indices {
                views_2d[usize::from(index)] = &**placeholder;
            }
        }
        let views_cube = lane_views(1);
        let views_3d = lane_views(2);
        let samplers: Vec<&WgpuSampler> = self.samplers.iter().map(|s| &**s).collect();
        device.create_bind_group(
            label,
            bgl,
            &[
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_2D,
                    resource: BindingResource::TextureViewArray(&views_2d),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_CUBE,
                    resource: BindingResource::TextureViewArray(&views_cube),
                },
                BindGroupEntry {
                    binding: TEXTURE_TABLE_BINDING_3D,
                    resource: BindingResource::TextureViewArray(&views_3d),
                },
                BindGroupEntry {
                    binding: d3d9_sm3::TEXTURE_TABLE_BINDING_SAMPLERS,
                    resource: BindingResource::SamplerArray(&samplers),
                },
            ],
        )
    }

    /// Everything [`Self::slot_word`] could otherwise ask the device for, done
    /// now on the device's thread: after this, handing out slot words needs no
    /// GPU calls, so draws can be prepared on worker threads.
    pub fn ready_for_lanes(&mut self, device: &RenderDevice) {
        self.ensure_placeholders(device);
        self.materialize_samplers(device);
    }

    /// Creates the samplers [`Self::slot_word`] reserved slots for. Browser
    /// bind groups are built only after this ([`Self::ensure_web_group`]), so none
    /// holds a placeholder for these slots.
    fn materialize_samplers(&mut self, device: &RenderDevice) {
        if self.pending_samplers.is_empty() {
            return;
        }
        // Reserved in order, after the samplers already made.
        for (index, sampler) in std::mem::take(&mut self.pending_samplers) {
            debug_assert_eq!(usize::from(index), self.samplers.len());
            self.samplers.push(device.create_sampler(&sampler.descriptor()));
        }
        self.binds = None;
    }

    /// Attaches the views of slots handed out on worker threads, on the
    /// device thread, before any bind group uses them. `uploaded` is the
    /// registry the slots were resolved from, unchanged since.
    pub fn attach_views(&mut self, uploaded: &RuntimeUploadedImageRegistry) {
        if self.pending_views.is_empty() {
            return;
        }
        for pending in std::mem::take(&mut self.pending_views) {
            let view = uploaded
                .view_for(pending.identity, pending.dimension)
                .filter(|view| view.id() == pending.id);
            if view.is_none() {
                use std::sync::atomic::{AtomicBool, Ordering};
                static WARNED: AtomicBool = AtomicBool::new(false);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    diag::warn!(
                        World,
                        "texture table: no view for {:?} to attach; its slot shows the placeholder",
                        pending.identity
                    );
                }
            }
            if let Some(slot) = self.views[pending.lane].get_mut(usize::from(pending.index)) {
                *slot = view.cloned();
            }
        }
        self.binds = None;
    }

    fn ensure_placeholders(&mut self, device: &RenderDevice) {
        let placeholders = self.placeholders.get_or_insert_with(|| {
            let make = |label, layers, dimension, view_dimension| {
                let texture = device.create_texture(&TextureDescriptor {
                    label: Some(label),
                    size: Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: layers,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension,
                    format: TextureFormat::R8Unorm,
                    usage: TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                texture.create_view(&TextureViewDescriptor {
                    label: Some(label),
                    dimension: Some(view_dimension),
                    ..default()
                })
            };
            Placeholders {
                views: [
                    make(
                        "iw4_texture_table_placeholder_2d",
                        1,
                        TextureDimension::D2,
                        TextureViewDimension::D2,
                    ),
                    make(
                        "iw4_texture_table_placeholder_cube",
                        6,
                        TextureDimension::D2,
                        TextureViewDimension::Cube,
                    ),
                    make(
                        "iw4_texture_table_placeholder_3d",
                        1,
                        TextureDimension::D3,
                        TextureViewDimension::D3,
                    ),
                ],
                sampler: device.create_sampler(&SamplerDescriptor {
                    label: Some("iw4_texture_table_placeholder_sampler"),
                    ..default()
                }),
            }
        });
        for (lane, view) in self.views.iter_mut().zip(placeholders.views.iter()) {
            if lane.is_empty() {
                lane.push(Some(view.clone()));
            }
        }
        if self.samplers.is_empty() {
            self.samplers.push(placeholders.sampler.clone());
        }
    }
}

/// Browser build: slot words keep the table lane in the texture half's top bits
/// (every lane's capacity fits below them).
#[cfg(target_arch = "wasm32")]
const WEB_LANE_SHIFT: usize = 14;

pub(super) fn array_count(count: u32) -> NonZeroU32 {
    NonZeroU32::new(count).expect("texture table capacities are non-zero")
}
