// Copyright 2019 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(clippy::single_match)]

use super::{
    display, error::Error, events::WaylandSource, keyboard, outputs, pointers, window::WindowHandle,
};

use crate::{backend, mouse, AppHandler, TimerToken};

use calloop;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    output::{OutputHandler, OutputState},
    reexports::client::{self, globals::registry_queue_init, EventQueue},
    registry::{ProvidesRegistryState, RegistryHandler, RegistryState},
    registry_handlers,
    seat::{SeatHandler, SeatState},
    shell::xdg::{window::WindowHandler, XdgShellState},
    shm::{ShmHandler, ShmState},
};

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BinaryHeap},
    rc::Rc,
    time::{Duration, Instant},
};

use crate::backend::shared::linux;
use client::protocol::wl_keyboard::WlKeyboard;
use wayland_cursor::CursorTheme;
use wayland_protocols::wlr::unstable::layer_shell::v1::client::zwlr_layer_shell_v1::ZwlrLayerShellV1;
use wayland_protocols::xdg_shell::client::xdg_positioner::XdgPositioner;
use wayland_protocols::xdg_shell::client::xdg_surface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timer(backend::shared::Timer<u64>);

impl Timer {
    pub(crate) fn new(id: u64, deadline: Instant) -> Self {
        Self(backend::shared::Timer::new(deadline, id))
    }

    pub(crate) fn id(self) -> u64 {
        self.0.data
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.0.deadline()
    }

    pub fn token(&self) -> TimerToken {
        self.0.token()
    }
}

impl std::cmp::Ord for Timer {
    /// Ordering is so that earliest deadline sorts first
    // "Earliest deadline first" that a std::collections::BinaryHeap will have the earliest timer
    // at its head, which is just what is needed for timer management.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.deadline().cmp(&other.0.deadline()).reverse()
    }
}

impl std::cmp::PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone)]
pub struct Application {
    pub(super) data: std::sync::Arc<Data>,
}

#[allow(dead_code)]
pub(crate) struct Data {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    event_queue: EventQueue<Data>,

    /// Handles to any surfaces that have been created.
    pub(super) handles: RefCell<im::OrdMap<u64, WindowHandle>>,

    /// Close flag
    pub(super) shutdown: Cell<bool>,
    /// The currently active surface, if any (by wayland object ID)
    pub(super) active_surface_id: RefCell<std::collections::VecDeque<u64>>,
    // Stuff for timers
    /// A calloop event source for timers. We always set it to fire at the next set timer, if any.
    pub(super) timer_handle: calloop::timer::TimerHandle<TimerToken>,
    /// We stuff this here until the event loop, then `take` it and use it.
    timer_source: RefCell<Option<calloop::timer::Timer<TimerToken>>>,
    /// Currently pending timers
    ///
    /// The extra data is the surface this timer is for.
    pub(super) timers: RefCell<BinaryHeap<Timer>>,

    pub(super) roundtrip_requested: RefCell<bool>,

    /// track if the display was flushed during the event loop.
    /// prevents double flushing unnecessarily.
    pub(super) display_flushed: RefCell<bool>,
    /// reference to the pointer events manager.
    pub(super) pointer: pointers::Pointer,
    /// reference to the keyboard events manager.
    keyboard: keyboard::Manager,
    //clipboard: clipboard::Manager,
}

impl Application {
    pub fn new() -> Result<Self, Error> {
        tracing::info!("wayland application initiated");

        let conn = client::Connection::connect_to_env()?;
        let (globals, mut event_queue) =
            registry_queue_init(&conn).map_err(|e| Error::global("wl_registry", 0, e))?;
        let qh: client::QueueHandle<Data> = event_queue.handle();

        let registry_state = RegistryState::new(&globals);
        let compositor_state =
            CompositorState::bind(&globals, &qh).map_err(|e| Error::bind("wl_compositor", e))?;
        let xdg_shell_state =
            XdgShellState::bind(&globals, &qh).map_err(|e| Error::bind("xdg_shell", e))?;
        let seat_state = SeatState::new(&globals, &qh);
        let output_state = OutputState::new(&globals, &qh);
        let shm_state = ShmState::bind(&globals, &qh).map_err(|e| Error::bind("shm", e))?;

        let timer_source = calloop::timer::Timer::new().unwrap();
        let timer_handle = timer_source.handle();

        // TODO the cursor theme size needs more refinement, it should probably be the size needed to
        // draw sharp cursors on the largest scaled monitor.
        let pointer = pointers::Pointer::new(
            CursorTheme::load(&conn, *shm_state.wl_shm(), 64)?,
            compositor_state.create_surface(&qh),
        );

        // We need to have keyboard events set up for our seats before the next roundtrip.
        let appdata = std::sync::Arc::new(Data {
            event_queue,
            registry_state,
            compositor_state,
            xdg_shell_state,
            seat_state,
            output_state,
            shm_state,
            handles: RefCell::new(im::OrdMap::new()),
            shutdown: Cell::new(false),
            active_surface_id: RefCell::new(std::collections::VecDeque::with_capacity(20)),
            timer_handle,
            timer_source: RefCell::new(Some(timer_source)),
            timers: RefCell::new(BinaryHeap::new()),
            display_flushed: RefCell::new(false),
            //pointer,
            pointer: todo!(),
            keyboard: keyboard::Manager::default(),
            //clipboard: clipboard::Manager::new(&env.display, &env.registry)?,
            //clipboard: todo!(),
            roundtrip_requested: RefCell::new(false),
        });

        Ok(Application { data: appdata })
    }

    pub fn run(mut self, _handler: Option<Box<dyn AppHandler>>) {
        tracing::info!("wayland event loop initiated");
        // NOTE if we want to call this function more than once, we will need to put the timer
        // source back.
        let timer_source = self.data.timer_source.borrow_mut().take().unwrap();
        let qh = self.data.event_queue.handle();
        // flush pending events (otherwise anything we submitted since sync will never be sent)
        //self.data.wayland.display.flush().unwrap();

        // Use calloop so we can epoll both wayland events and others (e.g. timers)
        let mut event_loop =
            calloop::EventLoop::try_new().expect("Failed to initialize the event loop");

        loop {
            // FIXME: busy loop
            event_loop
                .dispatch(Duration::from_millis(16), &mut self.data)
                .unwrap();

            if self.data.shutdown.get() {
                break;
            }
        }
    }

    pub fn quit(&self) {
        self.data.shutdown.set(true);
    }

    /*
    pub fn clipboard(&self) -> clipboard::Clipboard {
        clipboard::Clipboard::from(&self.data.clipboard)
    }
    */

    pub fn get_locale() -> String {
        linux::env::locale()
    }
}

impl Data {
    pub(crate) fn set_cursor(&self, cursor: &mouse::Cursor) {
        self.pointer.replace(cursor);
    }

    fn current_window_id(&self) -> u64 {
        static DEFAULT: u64 = 0_u64;
        *self.active_surface_id.borrow().get(0).unwrap_or(&DEFAULT)
    }

    pub(super) fn acquire_current_window(&self) -> Option<WindowHandle> {
        self.handles
            .borrow()
            .get(&self.current_window_id())
            .cloned()
    }

    fn handle_timer_event(&self, _token: TimerToken) {
        // Don't borrow the timers in case the callbacks want to add more.
        let mut expired_timers = Vec::with_capacity(1);
        let mut timers = self.timers.borrow_mut();
        let now = Instant::now();
        while matches!(timers.peek(), Some(timer) if timer.deadline() < now) {
            // timer has passed
            expired_timers.push(timers.pop().unwrap());
        }
        drop(timers);
        for expired in expired_timers {
            let win = match self.handles.borrow().get(&expired.id()).cloned() {
                Some(s) => s,
                None => {
                    // NOTE this might be expected
                    tracing::warn!(
                        "received event for surface that doesn't exist any more {:?} {:?}",
                        expired,
                        expired.id()
                    );
                    continue;
                }
            };
            // re-entrancy
            if let Some(data) = win.data() {
                data.handler.borrow_mut().timer(expired.token())
            }
        }

        for (_, win) in self.handles_iter() {
            if let Some(data) = win.data() {
                data.run_deferred_tasks()
            }
        }

        // Get the deadline soonest and queue it.
        if let Some(timer) = self.timers.borrow().peek() {
            self.timer_handle
                .add_timeout(timer.deadline() - now, timer.token());
        }
        // Now flush so the events actually get sent (we don't do this automatically because we
        // aren't in a wayland callback.
        //self.wayland.display.flush().unwrap();
    }

    /// Shallow clones surfaces so we can modify it during iteration.
    pub(super) fn handles_iter(&self) -> impl Iterator<Item = (u64, WindowHandle)> {
        self.handles.borrow().clone().into_iter()
    }
}

impl CompositorHandler for Data {
    fn scale_factor_changed(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        surface: &client::protocol::wl_surface::WlSurface,
        new_factor: i32,
    ) {
        todo!()
    }

    fn frame(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        surface: &client::protocol::wl_surface::WlSurface,
        time: u32,
    ) {
        todo!()
    }
}

impl OutputHandler for Data {
    fn output_state(&mut self) -> &mut OutputState {
        todo!()
    }

    fn new_output(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        output: client::protocol::wl_output::WlOutput,
    ) {
        todo!()
    }

    fn update_output(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        output: client::protocol::wl_output::WlOutput,
    ) {
        todo!()
    }

    fn output_destroyed(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        output: client::protocol::wl_output::WlOutput,
    ) {
        todo!()
    }
}

impl WindowHandler for Data {
    fn request_close(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        window: &smithay_client_toolkit::shell::xdg::window::Window,
    ) {
        todo!()
    }

    fn configure(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        window: &smithay_client_toolkit::shell::xdg::window::Window,
        configure: smithay_client_toolkit::shell::xdg::window::WindowConfigure,
        serial: u32,
    ) {
        todo!()
    }
}

impl SeatHandler for Data {
    fn seat_state(&mut self) -> &mut SeatState {
        todo!()
    }

    fn new_seat(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        seat: client::protocol::wl_seat::WlSeat,
    ) {
        todo!()
    }

    fn new_capability(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        seat: client::protocol::wl_seat::WlSeat,
        capability: smithay_client_toolkit::seat::Capability,
    ) {
        todo!()
    }

    fn remove_capability(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        seat: client::protocol::wl_seat::WlSeat,
        capability: smithay_client_toolkit::seat::Capability,
    ) {
        todo!()
    }

    fn remove_seat(
        &mut self,
        conn: &client::Connection,
        qh: &client::QueueHandle<Self>,
        seat: client::protocol::wl_seat::WlSeat,
    ) {
        todo!()
    }
}

impl ProvidesRegistryState for Data {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState,];
}

impl ShmHandler for Data {
    fn shm_state(&mut self) -> &mut ShmState {
        &mut self.shm_state
    }
}

delegate_compositor!(Data);
delegate_output!(Data);
delegate_seat!(Data);
delegate_xdg_shell!(Data);
delegate_registry!(Data);
delegate_shm!(Data);
