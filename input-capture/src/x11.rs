use std::{
    collections::HashSet,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
    thread,
};

use async_trait::async_trait;
use futures_core::Stream;
use tokio::sync::mpsc::{self, Receiver, Sender};
use x11::xlib::{
    self, ButtonPress, ButtonPressMask, ButtonRelease, ButtonReleaseMask, Display,
    GrabModeAsync, GrabSuccess, KeyPress, KeyRelease,
    PointerMotionMask, Window, XCloseDisplay, XDefaultRootWindow, XEvent, XFlush, XGrabKeyboard,
    XGrabPointer, XNextEvent, XOpenDisplay, XPending, XQueryPointer, XUngrabKeyboard,
    XUngrabPointer, XWarpPointer,
};

use input_event::{BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent};

use super::{Capture, CaptureError, CaptureEvent, Position, error::X11InputCaptureCreationError};

/// X11 keycodes are offset by 8 from Linux evdev scancodes.
/// This is because X11 keycodes reserve values 0-7 for special purposes.
const X11_KEYCODE_OFFSET: u32 = 8;

/// Discrete scroll value per single scroll tick (matches Windows/Linux conventions)
const SCROLL_DISCRETE_VALUE: i32 = 120;

/// Scroll wheel X11 button mappings
const SCROLL_UP_BUTTON: u32 = 4;
const SCROLL_DOWN_BUTTON: u32 = 5;
const SCROLL_LEFT_BUTTON: u32 = 6;
const SCROLL_RIGHT_BUTTON: u32 = 7;

/// Polling interval for cursor position checking in microseconds.
/// 16ms (~60fps) balances responsiveness with CPU usage.
const POLL_INTERVAL_US: u64 = 16000;

/// X11 Input Capture backend
pub struct X11InputCapture {
    event_rx: Receiver<(Position, CaptureEvent)>,
    request_tx: std::sync::mpsc::Sender<Request>,
    running: Arc<AtomicBool>,
}

enum Request {
    Create(Position),
    Destroy(Position),
    Release,
    Exit,
}

struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct X11State {
    display: *mut Display,
    root: Window,
    active_clients: HashSet<Position>,
    active_position: Option<Position>,
    captured: bool,
    entry_point: (i32, i32),
    prev_pos: Option<(i32, i32)>,
    screen_bounds: Rect,
}

impl X11State {
    fn new(display: *mut Display) -> Self {
        let root = unsafe { XDefaultRootWindow(display) };
        let screen_bounds = Self::get_screen_bounds(display);
        Self {
            display,
            root,
            active_clients: HashSet::new(),
            active_position: None,
            captured: false,
            entry_point: (0, 0),
            prev_pos: None,
            screen_bounds,
        }
    }

    fn get_screen_bounds(display: *mut Display) -> Rect {
        unsafe {
            let screen = xlib::XDefaultScreen(display);
            let width = xlib::XDisplayWidth(display, screen);
            let height = xlib::XDisplayHeight(display, screen);
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            }
        }
    }

    fn query_pointer(&self) -> (i32, i32) {
        unsafe {
            let mut root_return: Window = 0;
            let mut child_return: Window = 0;
            let mut root_x = 0;
            let mut root_y = 0;
            let mut win_x = 0;
            let mut win_y = 0;
            let mut mask = 0;
            XQueryPointer(
                self.display,
                self.root,
                &mut root_return,
                &mut child_return,
                &mut root_x,
                &mut root_y,
                &mut win_x,
                &mut win_y,
                &mut mask,
            );
            (root_x, root_y)
        }
    }

    fn warp_pointer(&self, x: i32, y: i32) {
        unsafe {
            XWarpPointer(self.display, 0, self.root, 0, 0, 0, 0, x, y);
            XFlush(self.display);
        }
    }

    fn grab_input(&mut self) -> bool {
        unsafe {
            let pointer_result = XGrabPointer(
                self.display,
                self.root,
                0,
                (ButtonPressMask | ButtonReleaseMask | PointerMotionMask) as u32,
                GrabModeAsync,
                GrabModeAsync,
                self.root,
                0,
                xlib::CurrentTime,
            );
            if pointer_result != GrabSuccess {
                log::warn!("Failed to grab pointer: {}", pointer_result);
                return false;
            }

            let keyboard_result = XGrabKeyboard(
                self.display,
                self.root,
                0,
                GrabModeAsync,
                GrabModeAsync,
                xlib::CurrentTime,
            );
            if keyboard_result != GrabSuccess {
                log::warn!("Failed to grab keyboard: {}", keyboard_result);
                XUngrabPointer(self.display, xlib::CurrentTime);
                return false;
            }

            XFlush(self.display);
            self.captured = true;
            true
        }
    }

    fn ungrab_input(&mut self) {
        if self.captured {
            unsafe {
                XUngrabPointer(self.display, xlib::CurrentTime);
                XUngrabKeyboard(self.display, xlib::CurrentTime);
                XFlush(self.display);
            }
            self.captured = false;
        }
    }

    fn check_boundary_crossing(&mut self, x: i32, y: i32) -> Option<Position> {
        let prev = self.prev_pos.unwrap_or((x, y));
        self.prev_pos = Some((x, y));

        if self.active_position.is_some() {
            return None;
        }

        let bounds = &self.screen_bounds;
        
        // Check if cursor moved across any boundary
        for pos in [Position::Left, Position::Right, Position::Top, Position::Bottom] {
            if !self.active_clients.contains(&pos) {
                continue;
            }

            let crossed = match pos {
                Position::Left => x <= bounds.x && prev.0 > bounds.x,
                Position::Right => x >= bounds.x + bounds.width - 1 && prev.0 < bounds.x + bounds.width - 1,
                Position::Top => y <= bounds.y && prev.1 > bounds.y,
                Position::Bottom => y >= bounds.y + bounds.height - 1 && prev.1 < bounds.y + bounds.height - 1,
            };

            if crossed {
                return Some(pos);
            }
        }

        None
    }
}

impl Drop for X11State {
    fn drop(&mut self) {
        self.ungrab_input();
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

fn event_thread(
    event_tx: Sender<(Position, CaptureEvent)>,
    request_rx: std::sync::mpsc::Receiver<Request>,
    running: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), X11InputCaptureCreationError>>,
) {
    // Open display in thread context
    let display = unsafe { XOpenDisplay(ptr::null()) };
    if display.is_null() {
        let _ = ready.send(Err(X11InputCaptureCreationError::OpenDisplay));
        return;
    }

    let mut state = X11State::new(display);
    let _ = ready.send(Ok(()));

    while running.load(Ordering::Relaxed) {
        // Process requests
        while let Ok(request) = request_rx.try_recv() {
            match request {
                Request::Create(pos) => {
                    log::debug!("X11: Creating capture for position {:?}", pos);
                    state.active_clients.insert(pos);
                }
                Request::Destroy(pos) => {
                    log::debug!("X11: Destroying capture for position {:?}", pos);
                    state.active_clients.remove(&pos);
                    if state.active_position == Some(pos) {
                        state.ungrab_input();
                        state.active_position = None;
                    }
                }
                Request::Release => {
                    log::debug!("X11: Releasing capture");
                    state.ungrab_input();
                    state.active_position = None;
                }
                Request::Exit => {
                    log::debug!("X11: Exiting event thread");
                    return;
                }
            }
        }

        // Query current pointer position
        let (x, y) = state.query_pointer();

        // Check for boundary crossing if not currently captured
        if state.active_position.is_none() {
            if let Some(pos) = state.check_boundary_crossing(x, y) {
                log::debug!("X11: Crossed boundary to {:?} at ({}, {})", pos, x, y);
                state.active_position = Some(pos);
                state.entry_point = (x, y);
                
                if state.grab_input() {
                    let _ = event_tx.blocking_send((pos, CaptureEvent::Begin));
                } else {
                    state.active_position = None;
                }
            }
        }

        // Process X11 events if captured
        if state.captured {
            unsafe {
                while XPending(state.display) > 0 {
                    let mut event: XEvent = std::mem::zeroed();
                    XNextEvent(state.display, &mut event);

                    if let Some(pos) = state.active_position {
                        if let Some(capture_event) = process_x11_event(&event) {
                            let _ = event_tx.blocking_send((pos, capture_event));
                        }
                    }
                }

                // Reset cursor to entry point to accumulate relative motion
                let (cx, cy) = state.query_pointer();
                if cx != state.entry_point.0 || cy != state.entry_point.1 {
                    let dx = cx - state.entry_point.0;
                    let dy = cy - state.entry_point.1;
                    
                    if let Some(pos) = state.active_position {
                        if dx != 0 || dy != 0 {
                            let motion_event = CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                                time: 0,
                                dx: dx as f64,
                                dy: dy as f64,
                            }));
                            let _ = event_tx.blocking_send((pos, motion_event));
                        }
                    }
                    
                    state.warp_pointer(state.entry_point.0, state.entry_point.1);
                }
            }
        }

        // Sleep briefly to avoid busy-waiting
        std::thread::sleep(std::time::Duration::from_micros(POLL_INTERVAL_US));
    }
}

fn process_x11_event(event: &XEvent) -> Option<CaptureEvent> {
    unsafe {
        match event.type_ {
            ButtonPress => {
                let button_event = event.button;
                let button = match button_event.button {
                    1 => BTN_LEFT,
                    2 => BTN_MIDDLE,
                    3 => BTN_RIGHT,
                    // Scroll wheel events - up/down for vertical, left/right for horizontal
                    SCROLL_UP_BUTTON => return Some(CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: 0, // Vertical axis
                        value: -SCROLL_DISCRETE_VALUE,
                    }))),
                    SCROLL_DOWN_BUTTON => return Some(CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: 0, // Vertical axis
                        value: SCROLL_DISCRETE_VALUE,
                    }))),
                    SCROLL_LEFT_BUTTON => return Some(CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: 1, // Horizontal axis
                        value: -SCROLL_DISCRETE_VALUE,
                    }))),
                    SCROLL_RIGHT_BUTTON => return Some(CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: 1, // Horizontal axis
                        value: SCROLL_DISCRETE_VALUE,
                    }))),
                    8 => BTN_BACK,
                    9 => BTN_FORWARD,
                    _ => return None,
                };
                Some(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                    time: button_event.time as u32,
                    button,
                    state: 1,
                })))
            }
            ButtonRelease => {
                let button_event = event.button;
                let button = match button_event.button {
                    1 => BTN_LEFT,
                    2 => BTN_MIDDLE,
                    3 => BTN_RIGHT,
                    // Scroll wheel events don't have release
                    SCROLL_UP_BUTTON | SCROLL_DOWN_BUTTON | SCROLL_LEFT_BUTTON | SCROLL_RIGHT_BUTTON => return None,
                    8 => BTN_BACK,
                    9 => BTN_FORWARD,
                    _ => return None,
                };
                Some(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                    time: button_event.time as u32,
                    button,
                    state: 0,
                })))
            }
            KeyPress => {
                let key_event = event.key;
                let key = key_event.keycode.saturating_sub(X11_KEYCODE_OFFSET);
                Some(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: key_event.time as u32,
                    key,
                    state: 1,
                })))
            }
            KeyRelease => {
                let key_event = event.key;
                let key = key_event.keycode.saturating_sub(X11_KEYCODE_OFFSET);
                Some(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: key_event.time as u32,
                    key,
                    state: 0,
                })))
            }
            _ => None,
        }
    }
}

impl X11InputCapture {
    pub fn new() -> std::result::Result<Self, X11InputCaptureCreationError> {
        let (event_tx, event_rx) = mpsc::channel(64);
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            event_thread(event_tx, request_rx, running_clone, ready_tx);
        });

        // Wait for thread to be ready
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(X11InputCaptureCreationError::OpenDisplay),
        }

        Ok(Self {
            event_rx,
            request_tx,
            running,
        })
    }
}

impl Drop for X11InputCapture {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.request_tx.send(Request::Exit);
    }
}

#[async_trait]
impl Capture for X11InputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.request_tx
            .send(Request::Create(pos))
            .map_err(|_| CaptureError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed")))?;
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.request_tx
            .send(Request::Destroy(pos))
            .map_err(|_| CaptureError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed")))?;
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.request_tx
            .send(Request::Release)
            .map_err(|_| CaptureError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "channel closed")))?;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.request_tx.send(Request::Exit);
        Ok(())
    }
}

impl Stream for X11InputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
