use async_trait::async_trait;
use std::ptr;
use x11::{
    xlib::{self, XCloseDisplay, XSync, CurrentTime},
    xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::X11EmulationCreationError};

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new() -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;

        // Verify XTest extension is available
        unsafe {
            let mut event_base: i32 = 0;
            let mut error_base: i32 = 0;
            let mut major_version: i32 = 0;
            let mut minor_version: i32 = 0;
            
            let has_xtest = xtest::XTestQueryExtension(
                display,
                &mut event_base,
                &mut error_base,
                &mut major_version,
                &mut minor_version,
            );
            
            if has_xtest == 0 {
                log::error!("XTest extension not available");
                XCloseDisplay(display);
                return Err(X11EmulationCreationError::OpenDisplay);
            }
            
            log::info!(
                "XTest extension version {}.{} available",
                major_version,
                minor_version
            );

            // Enable XTest to work even when input is grabbed by another client
            // This is important for XFCE4 and other desktop environments that may
            // have global keyboard grabs for shortcuts
            xtest::XTestGrabControl(display, 1);
            xlib::XFlush(display);
        }

        Ok(Self { display })
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            // -1 for screen_number means current screen
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, -1, CurrentTime);
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        unsafe {
            let x11_button = match button {
                BTN_RIGHT => 3,
                BTN_MIDDLE => 2,
                BTN_BACK => 8,
                BTN_FORWARD => 9,
                BTN_LEFT => 1,
                _ => 1,
            };
            xtest::XTestFakeButtonEvent(self.display, x11_button, state as i32, CurrentTime);
        };
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    fn emulate_scroll(&self, axis: u8, value: f64) {
        let direction = match axis {
            1 => {
                if value < 0.0 {
                    Self::SCROLL_LEFT
                } else {
                    Self::SCROLL_RIGHT
                }
            }
            _ => {
                if value < 0.0 {
                    Self::SCROLL_UP
                } else {
                    Self::SCROLL_DOWN
                }
            }
        };

        unsafe {
            xtest::XTestFakeButtonEvent(self.display, direction, 1, CurrentTime);
            xtest::XTestFakeButtonEvent(self.display, direction, 0, CurrentTime);
        }
    }

    fn emulate_key(&self, key: u32, state: u8) {
        let x11_key = key + 8; // xorg keycodes are shifted by 8
        log::trace!("X11 emulate_key: evdev={}, x11={}, state={}", key, x11_key, state);
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, x11_key, state as i32, CurrentTime);
        }
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            // Disable XTest grab control before closing
            xtest::XTestGrabControl(self.display, 0);
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.relative_motion(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    self.emulate_scroll(axis, value);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    self.emulate_scroll(axis, value as f64);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            Event::Keyboard(KeyboardEvent::Modifiers { .. }) => {
                // X11 doesn't need separate modifier handling as XTest handles this
                // through the key events themselves
            }
        }
        unsafe {
            xlib::XFlush(self.display);
            // Sync to ensure events are processed by the X server
            XSync(self.display, 0);
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }
}
