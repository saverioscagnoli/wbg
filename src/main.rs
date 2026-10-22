use image::RgbImage;
use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::delegate_registry;
use smithay_client_toolkit::output::OutputHandler;
use smithay_client_toolkit::output::OutputState;
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

struct Wallpaper {
    output: wl_output::WlOutput,
    layer: LayerSurface,
}

fn draw(pool: &mut SlotPool, img: &RgbImage, layer: &LayerSurface, w: u32, h: u32) {
    let stride = w as i32 * 4;
    let (buffer, canvas) = pool
        .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Xrgb8888)
        .expect("create buffer");

    // "Cover" scaling: fill the screen, crop the overflow, keep aspect ratio.
    let (iw, ih) = img.dimensions();
    let scale = f32::max(w as f32 / iw as f32, h as f32 / ih as f32);
    let off_x = (iw as f32 * scale - w as f32) / 2.0;
    let off_y = (ih as f32 * scale - h as f32) / 2.0;

    for (y, row) in canvas.chunks_exact_mut(stride as usize).enumerate() {
        let sy = (((y as f32 + off_y) / scale) as u32).min(ih - 1);
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let sx = (((x as f32 + off_x) / scale) as u32).min(iw - 1);
            let [r, g, b] = img.get_pixel(sx, sy).0;
            px.copy_from_slice(&[b, g, r, 0xFF]); // XRGB little-endian = B,G,R,X in memory
        }
    }

    let surface = layer.wl_surface();
    surface.damage_buffer(0, 0, w as i32, h as i32);
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
    image: RgbImage,
    wallpapers: Vec<Wallpaper>,
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

        self.wallpapers.push(Wallpaper { output, layer });
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

        draw(&mut self.pool, &self.image, layer, w, h);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
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

fn main() {
    let path = std::env::args().nth(1).expect("no path");
    let image = image::open(&path).expect("failed to load image").to_rgb8();

    let conn = Connection::connect_to_env().expect("no wayland");
    let (globals, mut queue) = registry_queue_init(&conn).unwrap();
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("no wl_compositor");
    let layer_shell =
        LayerShell::bind(&globals, &qh).expect("no wlr-layer-shell for this compostior");

    let shm = Shm::bind(&globals, &qh).expect("no wl_shm");
    let pool = SlotPool::new(4096, &shm).unwrap();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        image,
        wallpapers: Vec::new(),
    };

    loop {
        queue.blocking_dispatch(&mut app).unwrap();
    }
}
