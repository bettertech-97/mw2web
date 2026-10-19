use bevy::prelude::*;
use frame::ClientSet;

pub(crate) use crate::render_core::AudioScope;
use crate::runtime::AudioRuntime;

#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchEpoch(pub u64);

impl MatchEpoch {
    pub fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

pub(crate) fn register(app: &mut App) {
    app.init_resource::<net::FireVerdictState>();
    let verdicts = app.world().resource::<net::FireVerdictState>().clone();
    app.init_resource::<AudioRuntime>();
    app.world()
        .resource::<AudioRuntime>()
        .set_fire_verdicts(verdicts);
    app.init_resource::<MatchEpoch>()
        .add_systems(
            Update,
            publish_audio_context
                .in_set(ClientSet::Predict)
                .after(frame::OwnerEventsPublished),
        )
        .add_systems(
            PostUpdate,
            (
                submit_presented_audio,
                cancel_audio_on_exit.after(submit_presented_audio),
            ),
        );
    #[cfg(target_arch = "wasm32")]
    app.init_resource::<FrameStart>()
        .add_systems(First, mark_frame_start)
        .add_systems(PostUpdate, pump_browser_audio.after(submit_presented_audio));
}

/// The browser's stand-in for the audio threads: one control pass per frame
/// (the output stream opens on the first), then clip decoding within a
/// budget — generous while a map loads, small once it plays.
#[cfg(target_arch = "wasm32")]
fn pump_browser_audio(
    runtime: Res<AudioRuntime>,
    clips: Option<Res<crate::ClipStore>>,
    loading: Option<Res<assets::MapLoadProcess>>,
    started: Res<FrameStart>,
    mut health: Local<FrameHealth>,
) {
    runtime.pump();
    let clips_at = web_time::Instant::now();
    if let Some(clips) = clips {
        let loading = loading.as_ref().is_some_and(|process| !process.is_complete());
        let budget = std::time::Duration::from_millis(if loading { 12 } else { 3 });
        clips.service().pump(budget);
    }
    health.clips_ms += clips_at.elapsed().as_secs_f64() * 1000.0;
    health.tick(&runtime, started.0);
}

/// When this frame's schedules began, so the health line can split a frame
/// into page-thread work and the wait for the next animation frame.
#[derive(Resource, Default)]
struct FrameStart(Option<web_time::Instant>);

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn mark_frame_start(mut started: ResMut<FrameStart>) {
    started.0 = Some(web_time::Instant::now());
}

/// Page-thread frame health, one line every few seconds: how long frames
/// take, the worst one, how many ran past the audio ring, and how often the
/// worklet went silent. The browser has no profiler a player will open, so
/// this is what "choppy" gets measured with.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Default)]
struct FrameHealth {
    last: Option<web_time::Instant>,
    window_start: Option<web_time::Instant>,
    frames: u32,
    total_ms: f64,
    worst_ms: f64,
    over_50: u32,
    over_170: u32,
    busy_ms: f64,
    busy_worst_ms: f64,
    clips_ms: f64,
    mix_ns_seen: u64,
    control_ns_seen: u64,
    quanta_seen: u64,
    underruns_seen: u64,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl FrameHealth {
    fn tick(&mut self, runtime: &AudioRuntime, started: Option<web_time::Instant>) {
        let now = web_time::Instant::now();
        if let Some(started) = started {
            let busy = (now - started).as_secs_f64() * 1000.0;
            self.busy_ms += busy;
            self.busy_worst_ms = self.busy_worst_ms.max(busy);
        }
        if let Some(last) = self.last {
            let ms = (now - last).as_secs_f64() * 1000.0;
            self.frames += 1;
            self.total_ms += ms;
            self.worst_ms = self.worst_ms.max(ms);
            if ms > 50.0 {
                self.over_50 += 1;
            }
            if ms > 170.0 {
                self.over_170 += 1;
            }
            if ms > 120.0 {
                // Logged at the frame itself so the lines around it say what
                // the page thread was doing.
                diag::info!(Audio, "frame health: stall {ms:.0}ms");
            }
        }
        self.last = Some(now);
        let start = *self.window_start.get_or_insert(now);
        let elapsed = (now - start).as_secs_f64();
        if elapsed < 5.0 {
            return;
        }
        let stats = runtime.diagnostics();
        let underruns = stats.device_underruns.saturating_sub(self.underruns_seen);
        self.underruns_seen = stats.device_underruns;
        let mix_ms = stats.web_mix_ns.saturating_sub(self.mix_ns_seen) as f64 / 1e6;
        let control_ms = stats.web_control_ns.saturating_sub(self.control_ns_seen) as f64 / 1e6;
        let quanta = stats.web_quanta.saturating_sub(self.quanta_seen);
        self.mix_ns_seen = stats.web_mix_ns;
        self.control_ns_seen = stats.web_control_ns;
        self.quanta_seen = stats.web_quanta;
        if self.frames > 0 {
            let frames = f64::from(self.frames);
            diag::info!(
                Audio,
                "frame health: {:.1}s frames={} avg={:.1}ms (busy {:.1}ms) worst={:.0}ms (busy {:.0}ms) over50={} over170={} audio_underruns=+{} sources={} | audio/frame: mix {:.2}ms ({} quanta) control {:.2}ms clips {:.2}ms",
                elapsed,
                self.frames,
                self.total_ms / frames,
                self.busy_ms / frames,
                self.worst_ms,
                self.busy_worst_ms,
                self.over_50,
                self.over_170,
                underruns,
                stats.rendered_sources,
                mix_ms / frames,
                quanta,
                control_ms / frames,
                self.clips_ms / frames,
            );
        }
        self.clips_ms = 0.0;
        self.window_start = Some(now);
        self.frames = 0;
        self.total_ms = 0.0;
        self.worst_ms = 0.0;
        self.busy_ms = 0.0;
        self.busy_worst_ms = 0.0;
        self.over_50 = 0;
        self.over_170 = 0;
    }
}

pub(crate) fn publish_audio_context(
    epoch: Res<MatchEpoch>,
    runtime: Res<AudioRuntime>,
    verdicts: Option<Res<net::FireVerdictState>>,
    clips: Option<Res<crate::ClipStore>>,
    mut mix: Option<ResMut<crate::script_mix::ScriptAudioMix>>,
    mut channels: Option<ResMut<crate::script_mix::ChannelAudioMix>>,
    listeners: Query<&Transform, With<crate::AmbientListener>>,
    generation: Option<Res<frame::WorldGeneration>>,
    presented: Option<Res<net::PresentedSnapshot>>,
    events: Option<Res<net::EntityEventCursor>>,
    local: Option<Res<net::LocalPresentClient>>,
    adopted: Option<Res<net::LastAdoptedSnapshot>>,
    prediction: Option<Res<net::ClientPredictionState>>,
    view: Option<Res<frame::ViewSubject>>,
) {
    if let Some(verdicts) = verdicts {
        runtime.set_fire_verdicts(verdicts.clone());
    }
    runtime.set_match_epoch(epoch.0);
    runtime.set_event_context(
        generation
            .and_then(|generation| generation.0)
            .zip(presented.as_ref().and_then(|presented| {
                presented.snapshot().map(|snapshot| {
                    if view.as_ref().is_some_and(|view| view.in_killcam()) {
                        local
                            .as_ref()
                            .map_or(snapshot.tick, |local| snapshot.view_tick(local.0))
                            .0
                    } else {
                        adopted
                            .as_ref()
                            .and_then(|adopted| adopted.next())
                            .map_or(snapshot.tick.0, |latest| latest.tick.0.max(snapshot.tick.0))
                    }
                })
            }))
            .zip(events.as_ref().map(|events| events.timeline()))
            .map(|((world, tick), timeline)| crate::event::EventContext {
                local_life: local.as_ref().and_then(|local| {
                    let snapshot = if view.as_ref().is_some_and(|view| view.in_killcam()) {
                        presented.as_ref()?.snapshot()?
                    } else {
                        adopted
                            .as_ref()
                            .and_then(|adopted| adopted.next())
                            .or_else(|| presented.as_ref()?.snapshot())?
                    };
                    let meta = snapshot.meta.for_client(local.0)?;
                    (meta.lifecycle == sim::ClientLifecycle::Alive)
                        .then_some((local.0.0, meta.life_sequence.0))
                }),
                world,
                timeline,
                tick,
                owner_tick: prediction
                    .as_ref()
                    .and_then(|prediction| prediction.0.history().newest())
                    .map(|latest| latest.tick.0),
            }),
    );
    runtime.set_media_service(clips.as_ref().map(|clips| clips.service()));
    if let Some(mix) = mix.as_mut() {
        mix.reset_epoch(epoch.0);
    }
    if let Some(channels) = channels.as_mut() {
        channels.reset_epoch(epoch.0);
    }
    runtime.set_cue_mix(
        mix.zip(channels)
            .map(|(mix, channels)| crate::cue_execution::CueMix {
                epoch: epoch.0,
                gain: mix.gain.clone(),
                channels: channels.bindings(),
            }),
    );
    let mut listener = listeners.iter();
    let pose = listener.next();
    assert!(listener.next().is_none(), "more than one ambient listener");
    runtime.set_listener(pose.map(|pose| crate::spatial::ListenerSnapshot {
        origin_inches: crate::transform_inches(pose.translation),
        right: (pose.rotation * Vec3::X).to_array(),
    }));
}

fn cancel_audio_on_exit(mut exit: MessageReader<AppExit>, runtime: Res<AudioRuntime>) {
    if exit.read().count() == 0 {
        return;
    }
    runtime.cancel_all();
    diag::info!(
        Audio,
        "audio: admissions queue_full={} logical_budget={} physical_budget={} concurrency={} cancelled={} stale_scope={} output_unavailable={}",
        runtime.rejection_count(crate::AdmissionFailure::QueueFull),
        runtime.rejection_count(crate::AdmissionFailure::LogicalBudget),
        runtime.rejection_count(crate::AdmissionFailure::PhysicalBudget),
        runtime.rejection_count(crate::AdmissionFailure::Concurrency),
        runtime.rejection_count(crate::AdmissionFailure::Cancelled),
        runtime.rejection_count(crate::AdmissionFailure::StaleScope),
        runtime.rejection_count(crate::AdmissionFailure::OutputUnavailable)
    );
    let stats = runtime.diagnostics();
    diag::info!(
        Audio,
        "audio: exit frames={} device_blocks={} null_blocks={} busy_blocks={} underruns={} peak={:.6} source_revision={} sources={} source_pending_layers={} source_rendered={} source_virtual={} source_dropped={}",
        stats.audio_frame,
        stats.device_blocks,
        stats.null_blocks,
        stats.busy_blocks,
        stats.device_underruns,
        stats.peak,
        stats.source_revision,
        stats.logical_sources,
        stats.pending_source_layers,
        stats.rendered_sources,
        stats.virtual_sources,
        stats.dropped_sources
    );
}

fn submit_presented_audio(
    mut runtime: ResMut<AudioRuntime>,
    epoch: Res<MatchEpoch>,
    settings: Option<Res<frame::GameSettings>>,
    listeners: Query<&Transform, With<crate::AmbientListener>>,
    destructibles: Option<Res<crate::destructible_loops::DestructibleSources>>,
    map: Option<Res<crate::ambient::MapSources>>,
    menu: Option<Res<crate::frontend::MenuSources>>,
    shellshock: Option<Res<crate::shellshock::ShellshockSources>>,
    breath: Option<Res<crate::breath::BreathSources>>,
) {
    runtime.set_match_epoch(epoch.0);
    let mut listener = listeners.iter();
    let pose = listener.next();
    assert!(listener.next().is_none(), "more than one ambient listener");
    runtime.set_listener(pose.map(|pose| crate::spatial::ListenerSnapshot {
        origin_inches: crate::transform_inches(pose.translation),
        right: (pose.rotation * Vec3::X).to_array(),
    }));
    if let Some(settings) = settings {
        runtime.set_master_volume(settings.master_volume);
    }
    let mut desired = Vec::new();
    if let Some(destructibles) = destructibles {
        desired.extend(destructibles.desired.iter().cloned());
    }
    if let Some(map) = map {
        desired.extend(map.desired.iter().cloned());
    }
    if let Some(menu) = menu {
        desired.extend(menu.source.iter().cloned());
    }
    if let Some(shellshock) = shellshock {
        desired.extend(shellshock.source.iter().cloned());
    }
    if let Some(breath) = breath {
        desired.extend(breath.source.iter().cloned());
    }
    runtime.set_sources(desired);
}
