mod args;
mod ipc;
mod log;
mod util;

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use image::AnimationDecoder;
use image::ImageDecoder;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::LoopHandle;
use smithay_client_toolkit::reexports::calloop::RegistrationToken;
use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::channel::channel;
use smithay_client_toolkit::reexports::calloop::timer::TimeoutAction;
use smithay_client_toolkit::reexports::calloop::timer::Timer;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_output;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::protocol::wl_surface;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::registry::ProvidesRegistryState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::Anchor;
use smithay_client_toolkit::shell::wlr_layer::KeyboardInteractivity;
use smithay_client_toolkit::shell::wlr_layer::Layer;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shell::wlr_layer::LayerShellHandler;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::ShmHandler;
use smithay_client_toolkit::shm::slot::Buffer;
use smithay_client_toolkit::shm::slot::SlotPool;

use traccia::debug;
use traccia::error;
use traccia::info;
use traccia::warn;

use crate::args::Command;

struct Wallpaper {
    output: wl_output::WlOutput,
    name: Option<String>,
    layer: LayerSurface,
    viewport: WpViewport,
    size: Option<(u32, u32)>,
    scale: i32,
}

impl Drop for Wallpaper {
    fn drop(&mut self) {
        // must go before the surfae
        self.viewport.destroy();
    }
}

struct Diff {
    spans: Vec<(u32, u32)>,
    data: Vec<u8>,
    delay: Duration,
}

impl Diff {
    fn apply(&self, dst: &mut [u8], scratch: &mut [u8]) {
        let n = lz4_flex::decompress_into(&self.data, scratch).expect("corrupt frame");
        let mut src = &scratch[..n];

        for &(offset, len) in &self.spans {
            let (head, rest) = src.split_at(len as usize);
            dst[offset as usize..][..len as usize].copy_from_slice(head);
            src = rest;
        }
    }
}

struct Animation {
    frames: Vec<Diff>,
    scratch: Vec<u8>,
    buffers: [(Buffer, usize); 2],
    pool: SlotPool,
    size: (u32, u32),
    current: usize,
    timer: Option<(LoopHandle<'static, App>, RegistrationToken)>,
}

impl Animation {
    fn decode(path: &Path, shm: &Shm) -> Result<Animation, image::ImageError> {
        let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
        let (w, h) = decoder.dimensions();
        let len = (w * h * 4) as usize;

        let mut first = Vec::new();
        let mut prev = vec![0; len];
        let mut curr = vec![0; len];
        let mut frames = Vec::new();

        for frame in decoder.into_frames().filter_map(Result::ok) {
            let (num, den) = frame.delay().numer_denom_ms();
            let ms = if den == 0 { 0 } else { num / den };
            let delay = Duration::from_millis(if ms < 20 { 100 } else { ms as u64 });

            for (px, src) in curr.chunks_exact_mut(4).zip(frame.buffer().pixels()) {
                let [r, g, b, a] = src.0;
                let m = |c: u8| (c as u16 * a as u16 / 255) as u8;

                px.copy_from_slice(&[m(b), m(g), m(r), 0xFF]);
            }

            if first.is_empty() {
                first = curr.clone();
            }

            let (spans, data) = util::encode(&prev, &curr);
            frames.push(Diff { spans, data, delay });
            std::mem::swap(&mut prev, &mut curr);
        }

        if let Some(diff) = frames.first_mut() {
            (diff.spans, diff.data) = util::encode(&prev, &first);
        }

        drop(prev);
        drop(curr);

        let mut pool = SlotPool::new(len * 2, shm)
            .map_err(|e| image::ImageError::IoError(std::io::Error::other(e)))?;

        let mut buffer = || {
            let (buffer, canvas) = pool
                .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Xrgb8888)
                .expect("create buffer");

            if !first.is_empty() {
                canvas.copy_from_slice(&first);
            }

            (buffer, 0)
        };

        let buffers = [buffer(), buffer()];

        Ok(Self {
            frames,
            scratch: vec![0; len],
            buffers,
            pool,
            size: (w, h),
            current: 0,
            timer: None,
        })
    }

    fn advance(&mut self) -> Duration {
        self.current = (self.current + 1) % self.frames.len();
        self.frames[self.current].delay
    }

    fn present(&mut self, surfaces: &[wl_surface::WlSurface]) {
        if surfaces.is_empty() {
            return;
        }

        let Some((buffer, frame)) = self
            .buffers
            .iter_mut()
            .find(|(b, _)| b.canvas(&mut self.pool).is_some())
        else {
            return;
        };

        let canvas = buffer.canvas(&mut self.pool).expect("free buffer");

        while *frame != self.current {
            *frame = (*frame + 1) % self.frames.len();
            self.frames[*frame].apply(canvas, &mut self.scratch);
        }

        buffer.activate().expect("activate");

        for surface in surfaces {
            surface.attach(Some(buffer.wl_buffer()), 0, 0);
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            surface.commit();
        }
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        if let Some((handle, token)) = self.timer.take() {
            handle.remove(token);
        }
    }
}

struct Source {
    path: PathBuf,
    animation: Option<Animation>,
}

impl Source {
    fn open(path: PathBuf, shm: &Shm) -> Result<Source, image::ImageError> {
        let reader = ImageReader::open(&path)?.with_guessed_format()?;

        if reader.format() == Some(ImageFormat::Gif) {
            let animation = Animation::decode(&path, shm)?;

            if animation.frames.len() > 1 {
                return Ok(Source {
                    path,
                    animation: Some(animation),
                });
            }
        } else {
            reader.into_dimensions()?;
        }

        Ok(Source {
            path,
            animation: None,
        })
    }

    fn r#for<'a>(
        default: &'a Source,
        overrides: &'a HashMap<String, Source>,
        name: Option<&str>,
    ) -> &'a Self {
        name.and_then(|n| overrides.get(n)).unwrap_or(default)
    }
}

fn draw_image(
    shm: &Shm,
    path: &Path,
    wp: &Wallpaper,
    w: u32,
    h: u32,
) -> Result<(), image::ImageError> {
    let pw = w * wp.scale as u32;
    let ph = h * wp.scale as u32;
    let stride = pw as i32 * 4;

    let img = image::open(path)?;
    let scaled = img.resize_to_fill(pw, ph, FilterType::Lanczos3).into_rgb8();
    drop(img);

    let mut pool = SlotPool::new((stride * ph as i32) as usize, shm)
        .map_err(|e| image::ImageError::IoError(std::io::Error::other(e)))?;

    let (buffer, canvas) = pool
        .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Xrgb8888)
        .expect("create buffer");

    for (px, src) in canvas.chunks_exact_mut(4).zip(scaled.pixels()) {
        let [r, g, b] = src.0;
        px.copy_from_slice(&[b, g, r, 0xFF]);
    }

    let surface = wp.layer.wl_surface();
    surface.set_buffer_scale(wp.scale);
    surface.damage_buffer(0, 0, pw as i32, ph as i32);
    buffer.attach_to(surface).expect("attach");

    wp.viewport.set_source(-1.0, -1.0, -1.0, -1.0);
    wp.viewport.set_destination(-1, -1);

    wp.layer.commit();

    drop(scaled);
    util::trim();

    Ok(())
}

fn fit_viewport(animation: &Animation, wp: &Wallpaper, w: u32, h: u32) {
    let (iw, ih) = (animation.size.0 as f64, animation.size.1 as f64);
    let scale = (w as f64 / iw).max(h as f64 / ih);
    let (cw, ch) = (w as f64 / scale, h as f64 / scale);

    // source is in buffer coordinates
    wp.layer.wl_surface().set_buffer_scale(1);
    wp.viewport
        .set_source((iw - cw) / 2.0, (ih - ch) / 2.0, cw, ch);

    // logical size
    wp.viewport.set_destination(w as i32, h as i32);
}

fn draw(shm: &Shm, src: &Source, wp: &Wallpaper) -> Result<(), image::ImageError> {
    let Some((w, h)) = wp.size else { return Ok(()) };

    match &src.animation {
        None => draw_image(shm, &src.path, wp, w, h),
        Some(animation) => {
            fit_viewport(animation, wp, w, h);
            Ok(())
        }
    }
}

struct NoEvents;

impl Dispatch2<WpViewporter, App> for NoEvents {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewporter,
        _: <WpViewporter as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpViewport, App> for NoEvents {
    fn event(
        &self,
        _: &mut App,
        _: &WpViewport,
        _: <WpViewport as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

#[derive(Debug, Clone)]
enum Target {
    Default,
    Output(String),
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    viewporter: WpViewporter,
    handle: LoopHandle<'static, Self>,
    default: Source,
    overrides: HashMap<String, Source>,
    wallpapers: Vec<Wallpaper>,
}

impl App {
    fn source_mut(&mut self, t: &Target) -> Option<&mut Source> {
        match t {
            Target::Default => Some(&mut self.default),
            Target::Output(n) => self.overrides.get_mut(n),
        }
    }

    fn shows(&self, t: &Target, wp: &Wallpaper) -> bool {
        let name = wp.name.as_deref();

        match t {
            Target::Default => !name.is_some_and(|n| self.overrides.contains_key(n)),
            Target::Output(n) => name == Some(n.as_str()),
        }
    }

    fn surfaces(&self, target: &Target) -> Vec<wl_surface::WlSurface> {
        self.wallpapers
            .iter()
            .filter(|wp| wp.size.is_some() && self.shows(target, wp))
            .map(|wp| wp.layer.wl_surface().clone())
            .collect()
    }

    fn target_for(&self, name: Option<&str>) -> Target {
        match name {
            Some(n) if self.overrides.contains_key(n) => Target::Output(n.to_string()),
            _ => Target::Default,
        }
    }

    fn refresh(&mut self, taret: &Target) {
        let surface = self.surfaces(taret);

        if let Some(animation) = self.source_mut(taret).and_then(|s| s.animation.as_mut()) {
            animation.present(&surface);
        }
    }

    fn tick(&mut self, target: &Target) -> Option<Duration> {
        let surfaces = self.surfaces(target);
        let animation = self.source_mut(target)?.animation.as_mut()?;
        let delay = animation.advance();

        animation.present(&surfaces);
        Some(delay)
    }

    fn animate(&mut self, target: Target) {
        let handle = self.handle.clone();
        let Some(animation) = self.source_mut(&target).and_then(|s| s.animation.as_mut()) else {
            return;
        };

        let timer = Timer::from_duration(animation.frames[animation.current].delay);
        let t = target.clone();

        match handle.insert_source(timer, move |deadline, _, app: &mut App| {
            match app.tick(&t) {
                Some(delay) => TimeoutAction::ToInstant(deadline + delay),
                None => TimeoutAction::Drop,
            }
        }) {
            Ok(token) => animation.timer = Some((handle, token)),
            Err(e) => error!("failed to start animation: {}", e.error),
        }
    }

    fn redraw(&mut self, output: Option<&str>) {
        for wp in &self.wallpapers {
            let name = wp.name.as_deref();

            match output {
                Some(target) if name != Some(target) => continue,
                None if name.is_some_and(|n| self.overrides.contains_key(n)) => continue,
                _ => {}
            }

            let src = Source::r#for(&self.default, &self.overrides, name);

            if let Err(e) = draw(&self.shm, src, wp) {
                error!("failed to draw '{}': {}", src.path.display(), e);
            }
        }

        let target = self.target_for(output);
        self.refresh(&target);
    }

    pub fn set_default(&mut self, path: PathBuf) -> Result<(), image::ImageError> {
        let src = Source::open(path, &self.shm)?;
        let old = std::mem::replace(&mut self.default, src);

        info!("default: '{}'", self.default.path.display());

        self.redraw(None);
        self.animate(Target::Default);

        drop(old);
        util::trim();
        Ok(())
    }

    pub fn set_output(&mut self, name: String, path: PathBuf) -> Result<(), image::ImageError> {
        let src = Source::open(path, &self.shm)?;
        info!("{}: {}", name, src.path.display());

        if !self
            .wallpapers
            .iter()
            .any(|wp| wp.name.as_deref() == Some(&name))
        {
            warn!("output '{}' is not connected, keeping it for later", name);
        }

        let old = self.overrides.insert(name.clone(), src);

        self.redraw(Some(&name));
        self.animate(Target::Output(name));
        drop(old);
        util::trim();
        Ok(())
    }

    pub fn unset_output(&mut self, name: &str) {
        if let Some(old) = self.overrides.remove(name) {
            self.redraw(Some(name));
            drop(old);
            util::trim();
        }
    }

    pub fn describe(&self) -> String {
        use std::fmt::Write;

        let mut out = format!("* {}\n", self.default.path.display());

        for wp in &self.wallpapers {
            let name = wp.name.as_deref().unwrap_or("?");
            let src = Source::r#for(&self.default, &self.overrides, wp.name.as_deref());
            let _ = writeln!(out, "{} {}", name, src.path.display());
        }

        out
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let name = self.output_state.info(&output).and_then(|i| i.name);
        info!("new output: {}", name.as_deref().unwrap_or("<unnamed>"));

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("wallpaper"),
            Some(&output),
        );

        let viewport = self
            .viewporter
            .get_viewport(layer.wl_surface(), qh, NoEvents);

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);

        // empty input region so the pointer falls through to the compositor,
        // otherwise the cursor keeps whatever shape it had before entering
        match Region::new(&self.compositor) {
            Ok(region) => layer.set_input_region(Some(region.wl_region())),
            Err(e) => warn!("failed to create input region: {}", e),
        }

        layer.commit();

        self.wallpapers.push(Wallpaper {
            output,
            name,
            layer,
            viewport,
            scale: 1,
            size: None,
        });
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.wallpapers.retain(|w| w.output != output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.wallpapers
            .retain(|w| w.layer.wl_surface() != layer.wl_surface());
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;

        if w == 0 || h == 0 {
            return;
        }

        let Some(wp) = self
            .wallpapers
            .iter_mut()
            .find(|wp| wp.layer.wl_surface() == layer.wl_surface())
        else {
            return;
        };

        wp.size = Some((w, h));
        let src = Source::r#for(&self.default, &self.overrides, wp.name.as_deref());

        if let Err(e) = draw(&self.shm, src, wp) {
            error!("failed to draw '{}': {}", src.path.display(), e);
        }

        let name = wp.name.clone();
        let target = self.target_for(name.as_deref());
        self.refresh(&target);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Some(wp) = self
            .wallpapers
            .iter_mut()
            .find(|wp| wp.layer.wl_surface() == surface)
        {
            wp.scale = factor;

            let src = Source::r#for(&self.default, &self.overrides, wp.name.as_deref());

            if let Err(e) = draw(&self.shm, src, wp) {
                error!("failed to draw '{}': {}", src.path.display(), e);
            }

            let name = wp.name.clone();
            let target = self.target_for(name.as_deref());
            self.refresh(&target);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

delegate_registry!(App);
smithay_client_toolkit::delegate_dispatch2!(App);

fn start_daemon(path: PathBuf) {
    info!("using path '{}'", path.display());

    let sock = ipc::socket_path();
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            error!("socket path: {}", sock.display());
            error!("failed to bind socket: {}", e);
            return;
        }
    };

    debug!("socket path: '{}'", sock.display());

    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            error!("failed to establish connection: {}", e);
            return;
        }
    };

    let (globals, queue) = registry_queue_init(&conn).unwrap();
    let qh = queue.handle();

    let compositor = match CompositorState::bind(&globals, &qh) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to get compositor: {}", e);
            return;
        }
    };

    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(ls) => ls,
        Err(e) => {
            error!("failed to init layer shell: {}", e);
            return;
        }
    };

    let shm = match Shm::bind(&globals, &qh) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to bind shm: {}", e);
            return;
        }
    };

    let default = match Source::open(path, &shm) {
        Ok(s) => s,
        Err(e) => {
            error!("failed to open image: {}", e);
            return;
        }
    };

    let viewporter = match globals.bind::<WpViewporter, App, NoEvents>(&qh, 1..=1, NoEvents) {
        Ok(v) => v,
        Err(e) => {
            error!("failed to bind viewporter: {}", e);
            return;
        }
    };

    let mut event_loop: EventLoop<'static, App> = match EventLoop::try_new() {
        Ok(el) => el,
        Err(e) => {
            error!("failed to create event loop: {}", e);
            return;
        }
    };

    let handle = event_loop.handle();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        viewporter,
        handle: handle.clone(),
        default,
        overrides: HashMap::new(),
        wallpapers: Vec::new(),
    };

    app.animate(Target::Default);

    if let Err(e) = WaylandSource::new(conn.clone(), queue).insert(handle.clone()) {
        error!("failed to insert wayland source: {}", e);
        return;
    }

    let (tx, rx) = channel::<UnixStream>();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if tx.send(stream).is_err() {
                break;
            }
        }
    });

    if let Err(e) = handle.insert_source(rx, |event, _, app| {
        if let channel::Event::Msg(stream) = event {
            ipc::handle_client(app, stream);
        }
    }) {
        error!("failed to insert ipc source: {}", e);
        return;
    };

    let signal = event_loop.get_signal();

    if let Err(e) = event_loop.run(None, &mut app, |_| {
        if let Some(e) = conn.protocol_error() {
            error!("wayland protocol error: {}", e);
            signal.stop();
        }
    }) {
        error!("failed to run event loop: {}", e);
        return;
    }
}

fn main() {
    log::init();

    let command = match Command::parse() {
        Ok(cmd) => cmd,
        Err(e) => {
            eprintln!("{}", e);
            Command::print_usage();
            std::process::exit(2);
        }
    };

    match command {
        Command::Start(path) => {
            if ipc::request("get").is_ok() {
                error!("wbg is already running, use `wbg set <path>`");
                std::process::exit(1);
            }

            let path = std::fs::canonicalize(&path).unwrap_or(path);
            start_daemon(path);
        }
        Command::Get => match ipc::request("get") {
            Ok(resp) => print!("{resp}"),
            Err(_) => {
                error!("wbg is not running");
                std::process::exit(2);
            }
        },
        Command::Set { output, path } => {
            let path = match std::fs::canonicalize(&path) {
                Ok(p) => p,
                Err(e) => {
                    error!("{}: {}", path.display(), e);
                    std::process::exit(1);
                }
            };

            let msg = match output {
                Some(o) => format!("set-output\n{}\n{}", o, path.display()),
                None => format!("set\n{}", path.display()),
            };

            match ipc::request(&msg) {
                Ok(resp) if resp.starts_with("error") => {
                    error!("{}", resp);
                    std::process::exit(1);
                }
                Ok(_) => {}
                Err(_) => {
                    error!("wbg is not running");
                    std::process::exit(2);
                }
            }
        }
        Command::Unset(name) => match ipc::request(&format!("unset-output\n{}", name)) {
            Ok(resp) => debug!("{}", resp),
            Err(_) => {
                error!("wbg is not running");
                std::process::exit(2);
            }
        },
    }
}
