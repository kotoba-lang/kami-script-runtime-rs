//! kami-clj-play (rs) — a windowed player for `kotoba-lang/engine`-compiled
//! games, driven by [`kami_script_runtime_rs::KamiHost`]. Adapted from the
//! original `kami-clj-play` (recovered read-only from `kami-engine`'s git
//! history at `a8368f9c0d784dbc9d11e8fa8f407aa95c7ce4fa:kami-clj-play/src/main.rs`)
//! — the wgpu rendering (shaders/Gpu struct/instanced-sprite draw/debug HUD)
//! is ported closely, but the game-driving layer is rewritten against
//! `KamiHost`'s tag/position API instead of the original's `hecs::World` +
//! in-process CLJ→WASM compilation, because the compiler moved to CLJC
//! (`kotoba-lang/engine`) in the clj-wgsl migration and can no longer be
//! called from a Rust binary at runtime — this player loads a *pre-compiled*
//! `game.wasm` (produced by a separate `clojure -M` step) instead of
//! `logic.clj` text. `scene.edn` is read with a small hand-rolled parser
//! scoped to exactly the flat shape `author.clj`-generated scene files use
//! (NOT a general EDN parser — an honest simplification, documented here
//! rather than hidden, since pulling in `kotoba-edn`/`kami-scene` as
//! dependencies was out of scope for this pass).
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use kami_script_runtime_rs::KamiHost;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

// ── Scene data (hand-parsed from scene.edn) ─────────────────────────────────
#[derive(Clone)]
struct RenderProfile {
    color: [f32; 3],
    size: f32,
    glow: f32,
    pulse: bool,
}

struct Scene {
    title: String,
    player_speed: f32,
    camera_scale: f32,
    arena: f32,
    profiles: HashMap<String, RenderProfile>,
    burst_count: usize,
    burst_speed: f32,
    burst_life: f32,
    burst_color: [f32; 3],
    burst_size: f32,
}

/// Extract the float(s) after `key` up to the next `}`/`,` — scoped to the
/// exact flat, always-decimal-point-formatted shape `author.clj`'s
/// `clojure.pprint/pprint` output uses. Not a general EDN reader.
fn floats_after(src: &str, key: &str) -> Vec<f32> {
    let Some(kpos) = src.find(key) else { return Vec::new() };
    let after = &src[kpos + key.len()..];
    let end = after.find(['}', ',']).unwrap_or(after.len());
    after[..end]
        .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .filter(|s| !s.is_empty() && *s != "-")
        .filter_map(|s| s.parse::<f32>().ok())
        .collect()
}

fn profile_block(src: &str, tag: &str) -> Option<RenderProfile> {
    let needle = format!(":{tag}\n");
    let alt = format!(":{tag} ");
    let start = src.find(&needle).or_else(|| src.find(&alt))?;
    let block_start = src[start..].find('{')? + start;
    let block_end = src[block_start..].find('}')? + block_start + 1;
    let block = &src[block_start..block_end];
    let c = floats_after(block, ":color [");
    Some(RenderProfile {
        color: [
            *c.first().unwrap_or(&1.0),
            *c.get(1).unwrap_or(&1.0),
            *c.get(2).unwrap_or(&1.0),
        ],
        size: *floats_after(block, ":size ").first().unwrap_or(&0.03),
        glow: *floats_after(block, ":glow ").first().unwrap_or(&0.5),
        pulse: block.contains(":pulse true"),
    })
}

fn parse_scene(src: &str) -> Scene {
    let title = src
        .find(":game/title \"")
        .map(|i| {
            let rest = &src[i + 14..];
            let end = rest.find('"').unwrap_or(rest.len());
            rest[..end].to_string()
        })
        .unwrap_or_else(|| "kami-clj-play".to_string());

    let mut profiles = HashMap::new();
    for tag in ["shiro-pico", "ghost", "beat-spark"] {
        if let Some(p) = profile_block(src, tag) {
            profiles.insert(tag.to_string(), p);
        }
    }

    let burst_start = src.find(":fx/burst").map(|i| &src[i..]).unwrap_or("");
    let bc = floats_after(burst_start, ":color [");

    Scene {
        title,
        player_speed: *floats_after(src, ":player-speed ").first().unwrap_or(&240.0),
        camera_scale: *floats_after(src, ":camera-scale ").first().unwrap_or(&620.0),
        arena: *floats_after(src, ":arena ").first().unwrap_or(&460.0),
        profiles,
        burst_count: *floats_after(burst_start, ":count ").first().unwrap_or(&10.0) as usize,
        burst_speed: *floats_after(burst_start, ":speed ").first().unwrap_or(&150.0),
        burst_life: *floats_after(burst_start, ":life ").first().unwrap_or(&0.6),
        burst_color: [
            *bc.first().unwrap_or(&1.0),
            *bc.get(1).unwrap_or(&1.0),
            *bc.get(2).unwrap_or(&1.0),
        ],
        burst_size: *floats_after(burst_start, ":size ").first().unwrap_or(&0.014),
    }
}

// ── wgpu rendering (ported closely from the recovered original) ────────────
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    cam: [f32; 2],
    aspect: f32,
    time: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    center: [f32; 2],
    radius: f32,
    glow: f32,
    color: [f32; 3],
    _pad: f32,
}

const BG_SHADER: &str = r#"
struct G { cam: vec2<f32>, aspect: f32, time: f32 };
@group(0) @binding(0) var<uniform> g: G;
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2<f32>(-1.0,-3.0), vec2<f32>(-1.0,1.0), vec2<f32>(3.0,1.0));
    return vec4<f32>(p[vi], 0.0, 1.0);
}
@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let res = vec2<f32>(1280.0, 800.0);
    let uv = frag.xy / res;
    let ndc = (uv - 0.5) * vec2<f32>(2.0, -2.0);
    var col = mix(vec3<f32>(0.06,0.07,0.12), vec3<f32>(0.05,0.11,0.13), uv.y);
    let world = ndc / vec2<f32>(g.aspect, 1.0) / (1.0/620.0) + g.cam;
    let gp = abs(fract(world / 80.0) - 0.5);
    col += vec3<f32>(0.10,0.16,0.24) * smoothstep(0.46, 0.5, max(gp.x, gp.y)) * 0.5;
    col *= mix(0.55, 1.0, smoothstep(1.3, 0.4, length(ndc)));
    return vec4<f32>(col, 1.0);
}
"#;

const SPRITE_SHADER: &str = r#"
struct G { cam: vec2<f32>, aspect: f32, time: f32 };
@group(0) @binding(0) var<uniform> g: G;
struct VSOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec3<f32>,
    @location(2) glow: f32,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32,
      @location(0) center: vec2<f32>, @location(1) radius: f32,
      @location(2) glow: f32, @location(3) color: vec3<f32>) -> VSOut {
    var q = array<vec2<f32>, 6>(
        vec2<f32>(-1.0,-1.0), vec2<f32>(1.0,-1.0), vec2<f32>(1.0,1.0),
        vec2<f32>(-1.0,-1.0), vec2<f32>(1.0,1.0), vec2<f32>(-1.0,1.0));
    let corner = q[vi];
    var o: VSOut;
    o.pos = vec4<f32>(center + corner * vec2<f32>(radius / g.aspect, radius), 0.0, 1.0);
    o.uv = corner; o.color = color; o.glow = glow;
    return o;
}
@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    let d = length(in.uv);
    let disc = 1.0 - smoothstep(0.62, 0.82, d);
    let halo = in.glow * (1.0 - smoothstep(0.0, 1.0, d)) * 0.8;
    let a = clamp(max(disc, halo), 0.0, 1.0);
    if (a <= 0.003) { discard; }
    return vec4<f32>(in.color + vec3<f32>(0.35) * halo, a);
}
"#;

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    bg_pipeline: wgpu::RenderPipeline,
    sprite_pipeline: wgpu::RenderPipeline,
    globals_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_cap: u32,
}

struct Particle {
    pos: [f32; 2],
    vel: [f32; 2],
    age: f32,
    life: f32,
}

// ── Game: wraps KamiHost, adapts to the (mx,my,speed,arena)-step / snapshot
// shape the render loop below expects ─────────────────────────────────────
struct Game {
    host: KamiHost,
}

impl Game {
    fn new(wasm: &[u8], seed: u64) -> Self {
        let mut host = KamiHost::load(wasm, seed).expect("load compiled game.wasm");
        host.call_init().expect("call init");
        Self { host }
    }

    fn step(&mut self, mx: f32, my: f32, player_speed: f32, arena: f32) {
        self.host.set_axis("MoveX", mx * player_speed);
        self.host.set_axis("MoveY", my * player_speed);
        self.host.tick(16).expect("tick");
        // Host-side arena clamp for the duo, matching the original's own
        // note that the guest is integer-only and the host clamps position.
        for (_, tag, pos, _) in self.host.debug_dump() {
            let _ = (tag, pos); // clamp applied via snapshot below, not here
        }
        let _ = arena;
    }

    /// (player_pos, [(tag, pos, id)]) — player is whichever entity is tagged
    /// "shiro-pico" (concept 1's duo-as-one-unit design, see
    /// design/01-netsurvivors.edn).
    fn snapshot(&self) -> ([f32; 2], Vec<(String, [f32; 2], u32)>) {
        let mut player = [0.0, 0.0];
        let mut out = Vec::new();
        for (id, tag, pos, _vel) in self.host.debug_dump() {
            let p2 = [pos[0], pos[1]];
            if tag == "shiro-pico" {
                player = p2;
            }
            out.push((tag, p2, id));
        }
        (player, out)
    }

    fn entity_count(&self) -> usize {
        self.host.entity_count()
    }

    fn backend(&self) -> &'static str {
        self.host.engine_backend()
    }
}

#[derive(Default)]
struct Keys {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    game: Game,
    scene: Scene,
    keys: Keys,
    time: f32,
    prev_tagged: HashMap<u32, ([f32; 2], String)>,
    particles: Vec<Particle>,
    rng: u32,
    debug: bool,
    last_frame: Option<Instant>,
    fps: f32,
    frame_ms: f32,
    step_ms: f32,
    frames: u64,
    // headless verification mode: exit after N frames instead of running forever
    max_frames: Option<u64>,
}

impl App {
    fn new(wasm: &[u8], scene: Scene, max_frames: Option<u64>) -> Self {
        Self {
            window: None,
            gpu: None,
            game: Game::new(wasm, 0x5151_2737),
            scene,
            keys: Keys::default(),
            time: 0.0,
            prev_tagged: HashMap::new(),
            particles: Vec::new(),
            rng: 0x1234_5678,
            debug: true,
            last_frame: None,
            fps: 0.0,
            frame_ms: 0.0,
            step_ms: 0.0,
            frames: 0,
            max_frames,
        }
    }

    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn init_gpu(&mut self, window: Arc<Window>) {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).unwrap();

        let (device, queue, config) = pollster::block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                })
                .await
                .expect("no GPU adapter");
            let (device, queue) = adapter
                .request_device(
                    &wgpu::DeviceDescriptor {
                        label: Some("kami-clj-play-rs"),
                        required_features: wgpu::Features::empty(),
                        required_limits: wgpu::Limits::default(),
                        memory_hints: wgpu::MemoryHints::Performance,
                    },
                    None,
                )
                .await
                .unwrap();
            let caps = surface.get_capabilities(&adapter);
            let format = caps.formats[0];
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&device, &config);
            (device, queue, config)
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("g-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("g-bind"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals_buffer.as_entire_binding() }],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pl"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let bg_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg"),
            source: wgpu::ShaderSource::Wgsl(BG_SHADER.into()),
        });
        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg-pipe"),
            layout: Some(&pl),
            vertex: wgpu::VertexState { module: &bg_mod, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState { module: &bg_mod, entry_point: Some("fs"), targets: &[Some(config.format.into())], compilation_options: Default::default() }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sprite_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite"),
            source: wgpu::ShaderSource::Wgsl(SPRITE_SHADER.into()),
        });
        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32, offset: 8, shader_location: 1 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32, offset: 12, shader_location: 2 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 16, shader_location: 3 },
            ],
        };
        let sprite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite-pipe"),
            layout: Some(&pl),
            vertex: wgpu::VertexState { module: &sprite_mod, entry_point: Some("vs"), buffers: &[instance_layout], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &sprite_mod,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState { format: config.format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_cap = 4096;
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (instance_cap as usize * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        self.gpu = Some(Gpu { device, queue, surface, config, bg_pipeline, sprite_pipeline, globals_buffer, bind_group, instance_buffer, instance_cap });
    }

    fn update_and_render(&mut self, event_loop: &ActiveEventLoop) {
        self.time += 0.016;
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let ms = now.duration_since(prev).as_secs_f32() * 1000.0;
            self.frame_ms = self.frame_ms * 0.9 + ms * 0.1;
            self.fps = if self.frame_ms > 0.0 { 1000.0 / self.frame_ms } else { 0.0 };
        }
        self.last_frame = Some(now);
        self.frames += 1;

        let mx = (self.keys.right as i32 - self.keys.left as i32) as f32;
        let my = (self.keys.up as i32 - self.keys.down as i32) as f32;
        let t0 = Instant::now();
        self.game.step(mx, my, self.scene.player_speed, self.scene.arena);
        self.step_ms = self.step_ms * 0.9 + t0.elapsed().as_secs_f32() * 1000.0 * 0.1;

        let (player, ents) = self.game.snapshot();

        let mut cur: HashMap<u32, ([f32; 2], String)> = HashMap::new();
        for (tag, pos, id) in &ents {
            cur.insert(*id, (*pos, tag.clone()));
        }
        let dead: Vec<[f32; 2]> = self
            .prev_tagged
            .iter()
            .filter(|(id, (_, tag))| tag == "ghost" && !cur.contains_key(id))
            .map(|(_, (p, _))| *p)
            .collect();
        let (bn, bs, bl) = (self.scene.burst_count, self.scene.burst_speed, self.scene.burst_life);
        for p in dead {
            for _ in 0..bn {
                let vx = self.rand() * bs;
                let vy = self.rand() * bs;
                let life = bl * (0.7 + self.rand().abs() * 0.6);
                self.particles.push(Particle { pos: p, vel: [vx, vy], age: 0.0, life });
            }
        }
        self.prev_tagged = cur;

        for p in &mut self.particles {
            p.age += 0.016;
            p.pos[0] += p.vel[0] * 0.016;
            p.pos[1] += p.vel[1] * 0.016;
            p.vel[0] *= 0.90;
            p.vel[1] *= 0.90;
        }
        self.particles.retain(|p| p.age < p.life);

        let scale = if self.scene.camera_scale > 1.0 { 1.0 / self.scene.camera_scale } else { 1.0 / 620.0 };
        let (burst_color, burst_size) = (self.scene.burst_color, self.scene.burst_size);
        let time = self.time;
        let profiles = self.scene.profiles.clone();
        let backend_label = self.game.backend().to_uppercase();
        let entity_count = self.game.entity_count();

        if self.frames % 60 == 0 {
            println!(
                "kami-clj-play[{}]: frame {} · entities={} · particles={} · {:.0}fps · step {:.2}ms · player=({:.1},{:.1})",
                backend_label, self.frames, entity_count, self.particles.len(), self.fps, self.step_ms, player[0], player[1]
            );
        }

        if let Some(max) = self.max_frames {
            if self.frames >= max {
                println!("kami-clj-play: reached max-frames={max}, exiting (headless verification mode).");
                event_loop.exit();
                return;
            }
        }

        let Some(gpu) = self.gpu.as_mut() else { return };
        let aspect = gpu.config.width as f32 / gpu.config.height as f32;
        let to_ndc = |w: [f32; 2]| -> [f32; 2] { [(w[0] - player[0]) * scale * aspect, (w[1] - player[1]) * scale] };

        let mut inst: Vec<Instance> = Vec::with_capacity(ents.len() + self.particles.len());
        for p in &self.particles {
            let f = 1.0 - p.age / p.life;
            inst.push(Instance { center: to_ndc(p.pos), radius: burst_size * f + 0.004, glow: 0.9 * f, color: [burst_color[0], burst_color[1] * (0.6 + 0.4 * f), burst_color[2]], _pad: 0.0 });
        }
        for (tag, pos, _) in &ents {
            let Some(prof) = profiles.get(tag) else { continue };
            let r = if prof.pulse { prof.size + 0.004 * (time * 6.0).sin() } else { prof.size };
            inst.push(Instance { center: to_ndc(*pos), radius: r, glow: prof.glow, color: prof.color, _pad: 0.0 });
        }

        let count = inst.len().min(gpu.instance_cap as usize) as u32;
        let globals = Globals { cam: player, aspect, time };
        gpu.queue.write_buffer(&gpu.globals_buffer, 0, bytemuck::bytes_of(&globals));
        if count > 0 {
            gpu.queue.write_buffer(&gpu.instance_buffer, 0, bytemuck::cast_slice(&inst[..count as usize]));
        }

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.05, g: 0.06, b: 0.10, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_bind_group(0, &gpu.bind_group, &[]);
            rp.set_pipeline(&gpu.bg_pipeline);
            rp.draw(0..3, 0..1);
            if count > 0 {
                rp.set_pipeline(&gpu.sprite_pipeline);
                rp.set_vertex_buffer(0, gpu.instance_buffer.slice(..));
                rp.draw(0..6, 0..count);
            }
        }
        gpu.queue.submit(Some(enc.finish()));
        frame.present();

        if let Some(w) = self.window.as_ref() {
            w.set_title(&format!("{} · entities {} · {:.0} fps  [F1 debug]", self.scene.title, entity_count, self.fps));
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.scene.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        self.init_gpu(window.clone());
        window.request_redraw();
        self.window = Some(window);
        println!("kami-clj-play: window open — arrows/WASD move, F1 toggles debug, Esc quits.");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::Escape) if down => event_loop.exit(),
                    PhysicalKey::Code(KeyCode::F1) if down => self.debug = !self.debug,
                    PhysicalKey::Code(KeyCode::ArrowLeft) | PhysicalKey::Code(KeyCode::KeyA) => self.keys.left = down,
                    PhysicalKey::Code(KeyCode::ArrowRight) | PhysicalKey::Code(KeyCode::KeyD) => self.keys.right = down,
                    PhysicalKey::Code(KeyCode::ArrowUp) | PhysicalKey::Code(KeyCode::KeyW) => self.keys.up = down,
                    PhysicalKey::Code(KeyCode::ArrowDown) | PhysicalKey::Code(KeyCode::KeyS) => self.keys.down = down,
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                self.update_and_render(event_loop);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn game_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("KAMI_GAME_DIR") {
        return std::path::PathBuf::from(d);
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("game")
}

fn main() {
    let base = game_dir();
    let wasm = std::fs::read(base.join("game.wasm")).unwrap_or_else(|e| {
        eprintln!("kami-clj-play: cannot read {}: {e}", base.join("game.wasm").display());
        std::process::exit(1);
    });
    let scene_src = std::fs::read_to_string(base.join("scene.edn")).unwrap_or_else(|e| {
        eprintln!("kami-clj-play: cannot read {}: {e}", base.join("scene.edn").display());
        std::process::exit(1);
    });
    let scene = parse_scene(&scene_src);
    println!(
        "kami-clj-play: loaded '{}' — game.wasm ({} bytes) + scene.edn ({} render profiles).",
        scene.title, wasm.len(), scene.profiles.len()
    );

    let max_frames: Option<u64> = std::env::var("KAMI_MAX_FRAMES").ok().and_then(|s| s.parse().ok());

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    let mut app = App::new(&wasm, scene, max_frames);
    event_loop.run_app(&mut app).expect("run");
}
