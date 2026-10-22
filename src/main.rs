mod args;
mod ipc;

use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use image::DynamicImage;

use image::imageops::FilterType;
use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::channel::channel;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_output;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::protocol::wl_surface;
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
use smithay_client_toolkit::shm::slot::SlotPool;

use traccia::LevelFilter;
use traccia::debug;
use traccia::error;
use traccia::info;

use crate::args::Command;

struct Wallpaper {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    size: Option<(u32, u32)>,
    scale: i32,
}

fn draw(pool: &mut SlotPool, img: &DynamicImage, layer: &LayerSurface, w: u32, h: u32, scale: i32) {
    // Physical pixel size of the output.
    let pw = w * scale as u32;
    let ph = h * scale as u32;

    // Cover-scale and crop with a proper filter.
    let scaled = img.resize_to_fill(pw, ph, FilterType::Lanczos3).to_rgb8();

    let stride = pw as i32 * 4;
    let (buffer, canvas) = pool
        .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Xrgb8888)
        .expect("create buffer");

    for (px, src) in canvas.chunks_exact_mut(4).zip(scaled.pixels()) {
        let [r, g, b] = src.0;
        px.copy_from_slice(&[b, g, r, 0xFF]);
    }

    let surface = layer.wl_surface();
    surface.set_buffer_scale(scale);
    surface.damage_buffer(0, 0, pw as i32, ph as i32);
    buffer.attach_to(surface).expect("attach");
    layer.commit();
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    image: DynamicImage,
    path: PathBuf,
    wallpapers: Vec<Wallpaper>,
}

impl App {
    pub fn set_wallpaper(&mut self, path: PathBuf) -> Result<(), image::ImageError> {
        self.image = image::open(&path)?;
        self.path = path;
        info!("using path: '{}'", self.path.display());

        for wp in &self.wallpapers {
            if let Some((w, h)) = wp.size {
                draw(&mut self.pool, &self.image, &wp.layer, w, h, wp.scale);
            }
        }

        Ok(())
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("wallpaper"),
            Some(&output),
        );

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();

        self.wallpapers.push(Wallpaper {
            output,
            layer,
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

        let mut scale = 1;

        if let Some(wp) = self
            .wallpapers
            .iter_mut()
            .find(|wp| wp.layer.wl_surface() == layer.wl_surface())
        {
            wp.size = Some((w, h));
            scale = wp.scale;
        }
        draw(&mut self.pool, &self.image, layer, w, h, scale);
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

            if let Some((w, h)) = wp.size {
                draw(&mut self.pool, &self.image, &wp.layer, w, h, factor);
            }
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

    let image = match image::open(&path) {
        Ok(img) => img,
        Err(e) => {
            error!("failed to open image: {}", e);
            return;
        }
    };

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

    let pool = SlotPool::new(4096, &shm).unwrap();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        image,
        path,
        wallpapers: Vec::new(),
    };

    let mut event_loop: EventLoop<App> = match EventLoop::try_new() {
        Ok(el) => el,
        Err(e) => {
            error!("failed to create event loop: {}", e);
            return;
        }
    };

    let handle = event_loop.handle();

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

    if let Err(e) = event_loop.run(None, &mut app, |_| {}) {
        error!("failed to run event loop: {}", e);
        return;
    }
}

fn main() {
    let _ = traccia::init(
        traccia::Config::default().with_min_level(if cfg!(debug_assertions) {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        }),
    );

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
        Command::Set(path) => {
            let path = match std::fs::canonicalize(&path) {
                Ok(p) => p,
                Err(e) => {
                    error!("{}: {}", path.display(), e);
                    std::process::exit(1);
                }
            };

            match ipc::request(&format!("set {}", path.display())) {
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
    }
}
