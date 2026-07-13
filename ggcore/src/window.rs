//! Native window backend: winit event loop (pumped from Python) +
//! softbuffer surface. Python drives the loop: pump() returns input
//! events, present() blits the frame rendered by our rasterizer.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta,
                   WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

/// (kind, a, b, text)
pub type Event = (String, f64, f64, String);

pub struct WinApp {
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    pub events: Vec<Event>,
    title: String,
    init_size: (u32, u32),
    /// Physical (device px) size: this is what present() blits at.
    pub size: (u32, u32),
    /// DPI scale factor (device px per logical/CSS px). All event
    /// coordinates and sizes are reported to Python in logical px.
    pub scale: f64,
    mouse: (f64, f64),
}

impl WinApp {
    fn new(width: u32, height: u32, title: String) -> WinApp {
        WinApp {
            window: None,
            surface: None,
            events: Vec::new(),
            title,
            init_size: (width, height),
            size: (width, height),
            scale: 1.0,
            mouse: (0.0, 0.0),
        }
    }

    fn push(&mut self, kind: &str, a: f64, b: f64, text: &str) {
        self.events
            .push((kind.to_string(), a, b, text.to_string()));
    }

    fn to_logical(&self, v: f64) -> f64 {
        v / self.scale
    }

    fn record_resize(&mut self, width: u32, height: u32) {
        self.size = (width, height);
        self.push(
            "resize",
            self.to_logical(width as f64),
            self.to_logical(height as f64),
            "",
        );
    }

    fn record_scale_change(&mut self, scale: f64) {
        if scale > 0.0 {
            self.scale = scale;
        }
        // report as a resize: the logical size changed even though
        // the physical surface may not have (yet)
        self.push(
            "resize",
            self.to_logical(self.size.0 as f64),
            self.to_logical(self.size.1 as f64),
            "",
        );
    }

    pub fn present(&mut self, width: u32, height: u32, rgb: &[u8]) {
        let (Some(surface), Some(window)) =
            (self.surface.as_mut(), self.window.as_ref())
        else {
            return;
        };
        let (Some(w), Some(h)) =
            (NonZeroU32::new(width), NonZeroU32::new(height))
        else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let pixels = (width as usize * height as usize)
            .min(buffer.len())
            .min(rgb.len() / 3);
        for i in 0..pixels {
            let r = rgb[i * 3] as u32;
            let g = rgb[i * 3 + 1] as u32;
            let b = rgb[i * 3 + 2] as u32;
            buffer[i] = (r << 16) | (g << 8) | b;
        }
        buffer.present().ok();
        window.set_title(&self.title);
    }
}

impl ApplicationHandler for WinApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(LogicalSize::new(
                self.init_size.0,
                self.init_size.1,
            ));
        let Ok(window) = event_loop.create_window(attrs) else {
            self.push("error", 0.0, 0.0, "window creation failed");
            return;
        };
        let window = Arc::new(window);
        window.set_ime_allowed(true);
        self.scale = window.scale_factor();
        let phys = window.inner_size();
        self.size = (phys.width, phys.height);

        if let Ok(context) = softbuffer::Context::new(window.clone()) {
            if let Ok(surface) =
                softbuffer::Surface::new(&context, window.clone())
            {
                self.surface = Some(surface);
            }
        }
        self.window = Some(window);
        self.push(
            "ready",
            self.to_logical(self.size.0 as f64),
            self.to_logical(self.size.1 as f64),
            "",
        );
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => self.push("close", 0.0, 0.0, ""),
            WindowEvent::Resized(size) => {
                self.record_resize(size.width, size.height);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.record_scale_change(scale_factor);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = self.to_logical(position.x);
                let y = self.to_logical(position.y);
                self.mouse = (x, y);
                self.push("mouse_move", x, y, "");
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } => {
                let name = match button {
                    MouseButton::Left => "left",
                    MouseButton::Right => "right",
                    MouseButton::Middle => "middle",
                    _ => "other",
                };
                self.push("mouse_down", self.mouse.0, self.mouse.1, name);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        (x as f64 * 40.0, y as f64 * 90.0)
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        (self.to_logical(p.x), self.to_logical(p.y))
                    }
                };
                self.push("wheel", dx, dy, "");
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let named = match &event.logical_key {
                    Key::Named(NamedKey::Enter) => Some("Enter"),
                    Key::Named(NamedKey::Backspace) => Some("Backspace"),
                    Key::Named(NamedKey::Delete) => Some("Delete"),
                    Key::Named(NamedKey::ArrowLeft) => Some("ArrowLeft"),
                    Key::Named(NamedKey::ArrowRight) => Some("ArrowRight"),
                    Key::Named(NamedKey::ArrowUp) => Some("ArrowUp"),
                    Key::Named(NamedKey::ArrowDown) => Some("ArrowDown"),
                    Key::Named(NamedKey::Home) => Some("Home"),
                    Key::Named(NamedKey::End) => Some("End"),
                    Key::Named(NamedKey::PageUp) => Some("PageUp"),
                    Key::Named(NamedKey::PageDown) => Some("PageDown"),
                    Key::Named(NamedKey::Escape) => Some("Escape"),
                    Key::Named(NamedKey::Tab) => Some("Tab"),
                    _ => None,
                };
                if let Some(name) = named {
                    self.push("key", 0.0, 0.0, name);
                } else if let Some(text) = &event.text {
                    let clean: String = text
                        .as_str()
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect();
                    if !clean.is_empty() {
                        self.push("text", 0.0, 0.0, &clean);
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.push("text", 0.0, 0.0, &text);
            }
            _ => {}
        }
    }
}

pub struct NativeWindowInner {
    event_loop: EventLoop<()>,
    pub app: WinApp,
}

impl NativeWindowInner {
    pub fn new(
        width: u32,
        height: u32,
        title: String,
    ) -> Result<NativeWindowInner, String> {
        let mut builder = EventLoop::builder();
        #[cfg(target_os = "windows")]
        {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        }
        let event_loop = builder
            .build()
            .map_err(|e| format!("event loop: {e}"))?;
        Ok(NativeWindowInner {
            event_loop,
            app: WinApp::new(width, height, title),
        })
    }

    pub fn pump(&mut self, timeout_ms: u64) -> Vec<Event> {
        use winit::platform::pump_events::EventLoopExtPumpEvents;
        self.event_loop.pump_app_events(
            Some(Duration::from_millis(timeout_ms)),
            &mut self.app,
        );
        std::mem::take(&mut self.app.events)
    }

    pub fn set_title(&mut self, title: String) {
        self.app.title = title.clone();
        if let Some(w) = &self.app.window {
            w.set_title(&title);
        }
    }

    pub fn set_cursor_pointer(&mut self, pointer: bool) {
        if let Some(w) = &self.app.window {
            w.set_cursor(if pointer {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HiDPI regression: `size` (used for the present() buffer) stays
    /// physical while every reported event is in logical px.
    #[test]
    fn resize_reports_logical_keeps_physical() {
        let mut app = WinApp::new(800, 600, "t".into());
        app.scale = 2.0;
        app.record_resize(1600, 1200);
        assert_eq!(app.size, (1600, 1200));
        assert_eq!(
            app.events.pop().unwrap(),
            ("resize".into(), 800.0, 600.0, String::new())
        );
    }

    #[test]
    fn scale_change_reports_new_logical_size() {
        let mut app = WinApp::new(800, 600, "t".into());
        app.record_resize(1600, 1200);
        app.events.clear();
        app.record_scale_change(2.0);
        assert_eq!(app.scale, 2.0);
        assert_eq!(
            app.events.pop().unwrap(),
            ("resize".into(), 800.0, 600.0, String::new())
        );
        // bogus scale factors are ignored rather than divided by
        app.record_scale_change(0.0);
        assert_eq!(app.scale, 2.0);
    }

    #[test]
    fn pointer_coordinates_convert_to_logical() {
        let mut app = WinApp::new(800, 600, "t".into());
        app.scale = 1.5;
        assert_eq!(app.to_logical(300.0), 200.0);
        assert_eq!(app.to_logical(0.0), 0.0);
    }
}
