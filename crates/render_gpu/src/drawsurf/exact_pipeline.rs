use std::sync::{Arc, Mutex};

use bevy::mesh::VertexBufferLayout;
use bevy::platform::collections::HashMap;
use bevy::prelude::Resource;
use bevy::render::render_resource::{
    BindGroupLayout, BindGroupLayoutDescriptor, ColorTargetState, DepthStencilState,
    MultisampleState, PipelineCompilationOptions, PipelineLayoutDescriptor, PrimitiveState,
    RawFragmentState, RawRenderPipelineDescriptor, RawVertexBufferLayout, RawVertexState,
    RenderPipeline, ShaderModule, ShaderModuleDescriptor, ShaderSource,
};
use bevy::render::renderer::RenderDevice;
use bevy::tasks::{Task, futures_lite::future};

use super::sm3_wgsl::{PASS_VERTEX_ENTRY, ValidatedPassWgsl};
use render_material::PortId;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct ModuleKey {
    port: PortId,
    cached_lighting: bool,
}

#[derive(Clone)]
pub(super) enum ExactModuleSource {
    Port(Arc<ValidatedPassWgsl>),
    CachedLighting(Arc<str>),
}

impl ExactModuleSource {
    fn wgsl(&self) -> &str {
        match self {
            Self::Port(module) => &module.source,
            Self::CachedLighting(source) => source,
        }
    }
}

pub(super) struct ExactPipelinePlan {
    pub(super) label: String,
    pub(super) fragment_entry: String,
    pub(super) vertex_buffers: Vec<VertexBufferLayout>,
    pub(super) targets: Vec<Option<ColorTargetState>>,
    pub(super) primitive: PrimitiveState,
    pub(super) depth_stencil: DepthStencilState,
    pub(super) multisample: MultisampleState,
    pub(super) constants_layout: BindGroupLayout,
    pub(super) textures_layout: BindGroupLayout,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub(super) struct ExactPipelineSlot(u32);

enum SlotState {
    Building,
    Ready(RenderPipeline),
}

struct PortBuild {
    module: ModuleKey,
    created: Option<Arc<ShaderModule>>,
    pipelines: Vec<(ExactPipelineSlot, RenderPipeline)>,
}

#[derive(Resource, Default)]
pub(super) struct ExactPipelineRegistry {
    world: frame::WorldGeneration,
    slots: Vec<SlotState>,
    /// Per slot: its pipeline has finished compiling. Asking the pipeline
    /// itself reads a GPU handle, which on the threaded browser build only the
    /// device's thread may do; draws are prepared on worker threads too, so
    /// `poll` (device thread, every frame) answers it once and stores it here.
    confirmed: Vec<bool>,
    by_key: HashMap<super::colour_submit::ExactColourPipelineKey, ExactPipelineSlot>,
    modules: HashMap<ModuleKey, Arc<ShaderModule>>,

    queued: HashMap<
        ModuleKey,
        (
            ExactModuleSource,
            Vec<(ExactPipelineSlot, ExactPipelinePlan)>,
        ),
    >,
    jobs: Vec<Task<PortBuild>>,
    /// Pipelines handed to the browser whose compile is not over yet
    /// (`compile_limit` caps it).
    in_flight: usize,
    /// Per slot: the browser is done compiling it, ready or failed. A failed
    /// compile never becomes ready, so counting only ready ones kept failed
    /// pipelines in flight for good, and enough of them stopped every later
    /// compile: a load that never finished.
    settled: Vec<bool>,
    /// Modules a running job is creating: their next pipelines wait for the
    /// module rather than have the browser compile the shader again.
    creating: bevy::platform::collections::HashSet<ModuleKey>,

    layouts: Mutex<HashMap<BindGroupLayoutDescriptor, BindGroupLayout>>,

    discovered: Mutex<Vec<super::colour_submit::ExactColourPipelineKey>>,
}

impl ExactPipelineRegistry {
    pub(super) fn set_world(&mut self, world: frame::WorldGeneration) -> bool {
        if self.world == world {
            return false;
        }
        *self = Self {
            world,
            ..Self::default()
        };
        true
    }

    pub(super) fn bind_group_layout(
        &self,
        device: &RenderDevice,
        descriptor: &BindGroupLayoutDescriptor,
    ) -> BindGroupLayout {
        self.layouts
            .lock()
            .expect("exact pipeline layout cache is never poisoned")
            .entry(descriptor.clone())
            .or_insert_with_key(|descriptor| {
                device.create_bind_group_layout(descriptor.label.as_ref(), &descriptor.entries)
            })
            .clone()
    }

    pub(super) fn discover(&self, key: super::colour_submit::ExactColourPipelineKey) {
        self.discovered
            .lock()
            .expect("exact pipeline discovery list is never poisoned")
            .push(key);
    }

    pub(super) fn take_discovered(&mut self) -> Vec<super::colour_submit::ExactColourPipelineKey> {
        std::mem::take(
            &mut *self
                .discovered
                .lock()
                .expect("exact pipeline discovery list is never poisoned"),
        )
    }

    pub(super) fn slot(
        &self,
        key: &super::colour_submit::ExactColourPipelineKey,
    ) -> Option<ExactPipelineSlot> {
        self.by_key.get(key).copied()
    }

    /// A built pipeline the browser may still be compiling in the background
    /// (see `build_pipeline`) is not ready until it has finished.
    pub(super) fn ready(&self, slot: ExactPipelineSlot) -> Option<&RenderPipeline> {
        match self.slots.get(slot.0 as usize) {
            Some(SlotState::Ready(pipeline))
                if self.confirmed.get(slot.0 as usize).copied().unwrap_or(false) =>
            {
                Some(pipeline)
            }
            _ => None,
        }
    }

    pub(super) fn is_ready(&self, slot: ExactPipelineSlot) -> bool {
        self.ready(slot).is_some()
    }

    pub(super) fn request(
        &mut self,
        key: super::colour_submit::ExactColourPipelineKey,
        source: ExactModuleSource,
        plan: ExactPipelinePlan,
    ) -> ExactPipelineSlot {
        if let Some(slot) = self.by_key.get(&key).copied() {
            return slot;
        }
        let slot = ExactPipelineSlot(
            u32::try_from(self.slots.len()).expect("exact pipeline slot count fits u32"),
        );
        self.slots.push(SlotState::Building);
        self.confirmed.push(false);
        self.by_key.insert(key, slot);

        let module = ModuleKey {
            port: key.port,
            cached_lighting: matches!(source, ExactModuleSource::CachedLighting(_)),
        };
        self.queued
            .entry(module)
            .or_insert_with(|| (source, Vec::new()))
            .1
            .push((slot, plan));
        slot
    }

    pub(super) fn flush(&mut self, device: &RenderDevice) {
        if self.queued.is_empty() {
            return;
        }

        // Shader modules and pipelines: GPU objects, so the pool that may
        // make them (the engine thread's own on the threaded browser build).
        let pool = assets::gpu_pool();
        let limit = compile_limit();
        let modules: Vec<ModuleKey> = self.queued.keys().copied().collect();
        for module in modules {
            let room = limit.saturating_sub(self.in_flight);
            if room == 0 {
                break;
            }
            let existing = self.modules.get(&module).cloned();
            if existing.is_none() && self.creating.contains(&module) {
                continue;
            }
            let Some((source, plans)) = self.queued.get_mut(&module) else {
                continue;
            };
            let batch: Vec<_> = plans.drain(..plans.len().min(room)).collect();
            let source = source.clone();
            if plans.is_empty() {
                self.queued.remove(&module);
            }
            if existing.is_none() {
                self.creating.insert(module);
            }
            self.in_flight += batch.len();
            let device = device.clone();
            self.jobs.push(
                pool.spawn(async move { build_port(&device, module, source, existing, batch) }),
            );
        }
    }

    pub(super) fn poll(&mut self) {
        let mut index = 0;
        while index < self.jobs.len() {
            let Some(build) = future::block_on(future::poll_once(&mut self.jobs[index])) else {
                index += 1;
                continue;
            };

            drop(self.jobs.swap_remove(index));
            self.creating.remove(&build.module);
            if let Some(created) = build.created {
                self.modules.insert(build.module, created);
            }
            for (slot, pipeline) in build.pipelines {
                self.slots[slot.0 as usize] = SlotState::Ready(pipeline);
                if let Some(confirmed) = self.confirmed.get_mut(slot.0 as usize) {
                    *confirmed = false;
                }
            }
        }
        self.confirmed.resize(self.slots.len(), false);
        self.settled.resize(self.slots.len(), false);
        for ((state, confirmed), settled) in self
            .slots
            .iter()
            .zip(self.confirmed.iter_mut())
            .zip(self.settled.iter_mut())
        {
            let SlotState::Ready(pipeline) = state else {
                continue;
            };
            if !*confirmed {
                *confirmed = pipeline.is_ready();
            }
            if !*settled && (*confirmed || pipeline.is_settled()) {
                *settled = true;
                self.in_flight = self.in_flight.saturating_sub(1);
            }
        }
    }

    pub(super) fn module_n(&self) -> usize {
        self.modules.len()
    }

    pub(super) fn building_n(&self) -> usize {
        self.jobs.len()
    }
}

/// Pipelines the browser may compile at once. It compiles each one handed
/// over in the background on a core of its GPU process; handing over a
/// map's ~1000 at once took every core for seconds, 100% CPU for whoever was
/// loading. Half the logical processors leaves the machine usable. Desktop
/// builds compile on the engine's own pool and need no cap.
fn compile_limit() -> usize {
    if cfg!(target_arch = "wasm32") {
        (web_threads::logical_processors() / 2).max(2)
    } else {
        usize::MAX
    }
}

fn build_port(
    device: &RenderDevice,
    module: ModuleKey,
    source: ExactModuleSource,
    existing: Option<Arc<ShaderModule>>,
    plans: Vec<(ExactPipelineSlot, ExactPipelinePlan)>,
) -> PortBuild {
    let label = format!(
        "iw4_exact_colour/{:016x}/{}{}",
        module.port.vertex_program_hash,
        module.port.vertex_type,
        if module.cached_lighting {
            "/cached"
        } else {
            ""
        }
    );

    let (shader, created) = match existing {
        Some(shader) => (shader, None),
        None => {
            let shader = Arc::new(unsafe {
                device.create_shader_module(ShaderModuleDescriptor {
                    label: Some(&label),
                    source: ShaderSource::Wgsl(std::borrow::Cow::Borrowed(source.wgsl())),
                })
            });
            (shader.clone(), Some(shader))
        }
    };
    let pipelines = plans
        .into_iter()
        .map(|(slot, plan)| (slot, build_pipeline(device, &shader, &plan)))
        .collect();
    PortBuild {
        module,
        created,
        pipelines,
    }
}

fn build_pipeline(
    device: &RenderDevice,
    shader: &ShaderModule,
    plan: &ExactPipelinePlan,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some(&plan.label),
        bind_group_layouts: &[Some(&plan.constants_layout), Some(&plan.textures_layout)],
        immediate_size: 0,
    });
    let buffers: Vec<RawVertexBufferLayout> = plan
        .vertex_buffers
        .iter()
        .map(|buffer| RawVertexBufferLayout {
            array_stride: buffer.array_stride,
            attributes: &buffer.attributes,
            step_mode: buffer.step_mode,
        })
        .collect();
    let compilation_options = PipelineCompilationOptions {
        constants: &[],
        zero_initialize_workgroup_memory: false,
    };
    let descriptor = RawRenderPipelineDescriptor {
        label: Some(&plan.label),
        layout: Some(&layout),
        vertex: RawVertexState {
            module: shader,
            entry_point: Some(PASS_VERTEX_ENTRY),
            buffers: &buffers,
            compilation_options: compilation_options.clone(),
        },
        fragment: Some(RawFragmentState {
            module: shader,
            entry_point: Some(&plan.fragment_entry),
            targets: &plan.targets,
            compilation_options,
        }),
        primitive: plan.primitive,
        depth_stencil: Some(plan.depth_stencil.clone()),
        multisample: plan.multisample,
        multiview_mask: None,
        cache: None,
    };
    // The browser compiles pipelines one at a time unless asked to compile in
    // the background, where it spreads them over its cores: about 10x faster
    // for a map's ~1000 material pipelines. The registry waits for them to
    // finish and keeps `compile_limit` of them in flight.
    #[cfg(target_arch = "wasm32")]
    return RenderPipeline::from(
        device
            .wgpu_device()
            .create_render_pipeline_deferred(&descriptor),
    );
    #[cfg(not(target_arch = "wasm32"))]
    device.create_render_pipeline(&descriptor)
}
