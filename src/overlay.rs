//! Hiding the cursor on Wayland.
//!
//! A Wayland client cannot hide the global cursor; it can only choose the
//! cursor image for its own surfaces.  So to hide it we map a transparent,
//! full-screen layer-shell surface on the overlay layer of every output and
//! set a null cursor as soon as the pointer enters it.  The first motion,
//! button press or scroll on that surface destroys it again, which hands the
//! pointer (and its cursor) back to whatever is underneath.
//!
//! Not every compositor moves pointer focus when a surface goes away, so
//! before destroying the overlay we set the default cursor shape on it; the
//! cursor is then visible right away and the client underneath replaces it
//! on the next motion.
//!
//! Pointer events on the overlay come from the compositor after libinput has
//! processed them, so palm rejection and disable-while-typing on touchpads
//! keep working: a resting palm will not bring the cursor back.

use std::error::Error;
use std::os::fd::AsFd;

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::backend::ReadEventsGuard;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1};

pub struct Overlay {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
}

struct State {
    qh: QueueHandle<State>,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    viewporter: Option<wp_viewporter::WpViewporter>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    /// 1x1 transparent buffer, stretched over each output with a viewport.
    pixel: Option<wl_buffer::WlBuffer>,
    outputs: Vec<Output>,
    seats: Vec<Seat>,
    hidden: bool,
    /// Set when the pointer moved over the overlay; reported to the main loop.
    pointer_activity: bool,
}

struct Output {
    name: u32,
    wl: wl_output::WlOutput,
    cover: Option<Cover>,
}

/// The transparent surface covering one output while the cursor is hidden.
struct Cover {
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    viewport: Option<wp_viewport::WpViewport>,
    /// Full-size buffer, only used when the compositor lacks wp_viewporter.
    buffer: Option<(wl_buffer::WlBuffer, u32, u32)>,
}

struct Seat {
    name: u32,
    wl: wl_seat::WlSeat,
    pointer: Option<Pointer>,
}

struct Pointer {
    wl: wl_pointer::WlPointer,
    shape: Option<WpCursorShapeDeviceV1>,
    /// Serial and position of the pointer entering the overlay, while it is
    /// on it.  The position tells real motion apart from the compositor
    /// merely moving focus onto our surface.
    enter: Option<(u32, f64, f64)>,
}

impl Overlay {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let conn = Connection::connect_to_env()
            .map_err(|e| format!("can't connect to the Wayland display: {e}"))?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let compositor =
            globals.bind(&qh, 1..=4, ()).map_err(|_| "compositor has no wl_compositor")?;
        let shm = globals.bind(&qh, 1..=1, ()).map_err(|_| "compositor has no wl_shm")?;
        let layer_shell = globals.bind(&qh, 1..=4, ()).map_err(|_| {
            "compositor doesn't support wlr-layer-shell (Weston doesn't), \
             so the cursor can't be hidden"
        })?;
        let viewporter = globals.bind(&qh, 1..=1, ()).ok();
        let cursor_shape = globals.bind(&qh, 1..=1, ()).ok();

        let mut state = State {
            qh: qh.clone(),
            compositor,
            shm,
            layer_shell,
            viewporter,
            cursor_shape,
            pixel: None,
            outputs: Vec::new(),
            seats: Vec::new(),
            hidden: false,
            pointer_activity: false,
        };
        if state.viewporter.is_some() {
            state.pixel = Some(state.shm_buffer(1, 1)?);
        } else {
            debug!("no wp_viewporter, using full-size buffers");
        }
        if state.cursor_shape.is_none() {
            debug!("no wp_cursor_shape, cursor may only return on the next motion");
        }

        let registry = globals.registry();
        for g in globals.contents().clone_list() {
            state.add_global(registry, g.name, &g.interface, g.version);
        }
        queue.roundtrip(&mut state)?;

        if state.seats.iter().all(|s| s.pointer.is_none()) {
            debug!("no seat with a pointer yet");
        }
        Ok(Overlay { conn, queue, state })
    }

    pub fn hide(&mut self) {
        self.state.hide();
    }

    pub fn show(&mut self) {
        self.state.show();
    }

    pub fn is_hidden(&self) -> bool {
        self.state.hidden
    }

    /// Whether the pointer moved over the overlay since the last call.
    pub fn take_pointer_activity(&mut self) -> bool {
        std::mem::take(&mut self.state.pointer_activity)
    }

    pub fn dispatch_pending(&mut self) -> Result<(), Box<dyn Error>> {
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<(), Box<dyn Error>> {
        match self.conn.flush() {
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(())
            }
            r => Ok(r?),
        }
    }

    /// Returns None if events are already queued and must be dispatched first.
    pub fn prepare_read(&self) -> Option<ReadEventsGuard> {
        self.queue.prepare_read()
    }
}

impl State {
    fn add_global(&mut self, registry: &wl_registry::WlRegistry, name: u32, iface: &str, ver: u32) {
        if iface == wl_output::WlOutput::interface().name {
            let wl = registry.bind(name, ver.min(4), &self.qh, ());
            self.outputs.push(Output { name, wl, cover: None });
            if self.hidden {
                self.cover(self.outputs.len() - 1);
            }
        } else if iface == wl_seat::WlSeat::interface().name {
            let wl = registry.bind(name, ver.min(5), &self.qh, ());
            self.seats.push(Seat { name, wl, pointer: None });
        }
    }

    fn remove_global(&mut self, name: u32) {
        if let Some(i) = self.outputs.iter().position(|o| o.name == name) {
            let mut out = self.outputs.remove(i);
            if let Some(cover) = out.cover.take() {
                cover.destroy();
            }
            if out.wl.version() >= 3 {
                out.wl.release();
            }
        } else if let Some(i) = self.seats.iter().position(|s| s.name == name) {
            let seat = self.seats.remove(i);
            if let Some(p) = seat.pointer {
                p.release();
            }
            if seat.wl.version() >= 5 {
                seat.wl.release();
            }
        }
    }

    fn hide(&mut self) {
        if self.hidden {
            return;
        }
        debug!("hiding cursor");
        self.hidden = true;
        for i in 0..self.outputs.len() {
            self.cover(i);
        }
    }

    fn show(&mut self) {
        if !self.hidden {
            return;
        }
        debug!("showing cursor");
        self.hidden = false;
        for p in self.seats.iter_mut().filter_map(|s| s.pointer.as_mut()) {
            if let (Some(shape), Some((serial, ..))) = (&p.shape, p.enter.take()) {
                shape.set_shape(serial, Shape::Default);
            }
        }
        for out in &mut self.outputs {
            if let Some(cover) = out.cover.take() {
                cover.destroy();
            }
        }
    }

    /// Creates the overlay for one output; it is mapped once configured.
    fn cover(&mut self, i: usize) {
        let out = &mut self.outputs[i];
        if out.cover.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(&self.qh, ());
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&surface, &self.qh, ()));
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            Some(&out.wl),
            Layer::Overlay,
            "wbanish".into(),
            &self.qh,
            (),
        );
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        // Cover panels and other exclusive zones too.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        out.cover = Some(Cover { surface, layer, viewport, buffer: None });
    }

    fn configure(&mut self, layer: &ZwlrLayerSurfaceV1, serial: u32, w: u32, h: u32) {
        let Some(i) =
            self.outputs.iter().position(|o| o.cover.as_ref().is_some_and(|c| &c.layer == layer))
        else {
            return;
        };
        layer.ack_configure(serial);
        if w == 0 || h == 0 {
            return;
        }

        let new_buffer = match (&self.pixel, &self.outputs[i].cover) {
            (Some(_), _) => None,
            (None, Some(Cover { buffer: Some((_, bw, bh)), .. })) if (*bw, *bh) == (w, h) => None,
            (None, _) => match self.shm_buffer(w, h) {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("wbanish: can't create buffer: {e}");
                    return;
                }
            },
        };

        let cover = self.outputs[i].cover.as_mut().unwrap();
        if let Some(vp) = &cover.viewport {
            vp.set_destination(w as i32, h as i32);
        }
        if let Some(b) = new_buffer
            && let Some((old, ..)) = cover.buffer.replace((b, w, h))
        {
            old.destroy();
        }
        let buffer = self.pixel.as_ref().or(cover.buffer.as_ref().map(|(b, ..)| b));
        cover.surface.attach(buffer, 0, 0);
        cover.surface.damage(0, 0, i32::MAX, i32::MAX);
        cover.surface.commit();
    }

    fn closed(&mut self, layer: &ZwlrLayerSurfaceV1) {
        for out in &mut self.outputs {
            if out.cover.as_ref().is_some_and(|c| &c.layer == layer) {
                out.cover.take().unwrap().destroy();
            }
        }
    }

    /// A fully transparent ARGB buffer; a fresh memfd is already zeroed.
    fn shm_buffer(&self, w: u32, h: u32) -> Result<wl_buffer::WlBuffer, Box<dyn Error>> {
        let stride = w.checked_mul(4).ok_or("buffer too large")?;
        let size = i32::try_from(stride as u64 * h as u64)?;
        let fd = memfd_create("wbanish", MemfdFlags::CLOEXEC)?;
        ftruncate(&fd, size as u64)?;
        let pool = self.shm.create_pool(fd.as_fd(), size, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        Ok(buffer)
    }

    fn pointer(&mut self, pointer: &wl_pointer::WlPointer) -> Option<&mut Pointer> {
        self.seats.iter_mut().filter_map(|s| s.pointer.as_mut()).find(|p| &p.wl == pointer)
    }

    fn pointer_moved(&mut self) {
        self.pointer_activity = true;
        self.show();
    }
}

impl Pointer {
    fn release(self) {
        if let Some(shape) = self.shape {
            shape.destroy();
        }
        if self.wl.version() >= 3 {
            self.wl.release();
        }
    }
}

impl Cover {
    fn destroy(self) {
        self.layer.destroy();
        if let Some(vp) = self.viewport {
            vp.destroy();
        }
        self.surface.destroy();
        if let Some((b, ..)) = self.buffer {
            b.destroy();
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                state.add_global(registry, name, &interface, version)
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_global(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event else {
            return;
        };
        let Some(s) = state.seats.iter_mut().find(|s| &s.wl == seat) else {
            return;
        };
        let has_pointer = caps.contains(wl_seat::Capability::Pointer);
        if has_pointer && s.pointer.is_none() {
            let wl = seat.get_pointer(qh, ());
            let shape = state.cursor_shape.as_ref().map(|m| m.get_pointer(&wl, qh, ()));
            s.pointer = Some(Pointer { wl, shape, enter: None });
        } else if !has_pointer && let Some(p) = s.pointer.take() {
            p.release();
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We only ever get pointer events for our own surfaces, the overlays.
        match event {
            // A late enter for an overlay we already destroyed.
            wl_pointer::Event::Enter { .. } if !state.hidden => {}
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                pointer.set_cursor(serial, None, 0, 0);
                if let Some(p) = state.pointer(pointer) {
                    p.enter = Some((serial, surface_x, surface_y));
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if let Some(p) = state.pointer(pointer) {
                    p.enter = None;
                }
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                let moved = state
                    .pointer(pointer)
                    .and_then(|p| p.enter)
                    .is_some_and(|(_, x, y)| (x, y) != (surface_x, surface_y));
                if moved {
                    state.pointer_moved();
                }
            }
            wl_pointer::Event::Button {
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            }
            | wl_pointer::Event::Axis { .. } => state.pointer_moved(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                state.configure(layer, serial, width, height)
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed(layer),
            _ => {}
        }
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: wp_viewporter::WpViewporter);
delegate_noop!(State: wp_viewport::WpViewport);
delegate_noop!(State: WpCursorShapeManagerV1);
delegate_noop!(State: WpCursorShapeDeviceV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
