//! Linux webview backend: a `WebKitGTK` view rendered off-screen and composited
//! over the 3D scene as a texture.
//!
//! `WebKitGTK` cannot paint transparent pixels into an on-screen X window (see
//! `wry#1667`), so a child-window overlay is opaque and hides the renderer.
//! Rendered into a `GtkOffscreenWindow` with an RGBA visual, the same engine
//! produces correct per-pixel alpha, which viso blends over the scene through
//! [`foldit_core::App::set_overlay_texture`].
//!
//! The view is never mapped, so it receives no input of its own: winit events
//! are synthesized into `GdkEvent`s and dispatched through GTK's normal routing
//! ([`input`]), which is what makes pages see `isTrusted === true`.
//!
//! * [`capture`] — offscreen surface → premultiplied BGRA texture.
//! * [`input`] — winit events → `GdkEvent` → `gtk_main_do_event`.

mod capture;
mod input;

use std::sync::mpsc::Sender;

use foldit_gui::IpcMessage;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use viso::wgpu;
use webkit2gtk::{
    SecurityManagerExt, URISchemeRequest, URISchemeRequestExt, UserContentInjectedFrames,
    UserContentManager, UserContentManagerExt, UserScript, UserScriptInjectionTime, WebContext,
    WebContextExt, WebView, WebViewExt, WebViewExtManual,
};
use winit::dpi::PhysicalSize;

use capture::Capture;
use input::InputState;

/// Name of the script-message handler backing `window.ipc.postMessage`. Must
/// match [`BRIDGE_SCRIPT`].
const IPC_HANDLER: &str = "ipc";

/// Installs `window.ipc.postMessage`, the channel the frontend posts on.
/// Injected before the page's own scripts evaluate.
const BRIDGE_SCRIPT: &str = "window.ipc = { postMessage: function (m) { \
     window.webkit.messageHandlers.ipc.postMessage(m); } };";

/// Resolves a request path to a response body and its MIME type, or `None` for
/// a 404.
pub type Resolver = Box<dyn Fn(&str) -> Option<(Vec<u8>, String)> + 'static>;

/// Where the page comes from.
pub struct Content {
    /// URL loaded once the view exists: a dev server, or an entry point under
    /// [`Self::scheme`].
    pub entry: String,
    /// An in-process handler for a custom URI scheme, registered before the
    /// load. `None` when `entry` points somewhere `WebKit` can already reach.
    pub scheme: Option<Scheme>,
}

/// Serves one custom URI scheme out of process memory.
pub struct Scheme {
    pub name: String,
    pub resolve: Resolver,
}

/// An off-screen `WebKitGTK` view plus the wgpu texture its frames land in.
pub struct OffscreenWebview {
    window: gtk::OffscreenWindow,
    webview: WebView,
    capture: Capture,
    input: InputState,
}

impl OffscreenWebview {
    /// Build the view at `size` physical pixels. `init_script` is injected at
    /// document start, alongside the `window.ipc` bridge; decoded IPC messages
    /// are sent on `ipc_tx`.
    ///
    /// Must be called on the GTK main thread, after `gtk::init()`.
    pub fn new(
        size: PhysicalSize<u32>,
        content: Content,
        init_script: &str,
        ipc_tx: Sender<IpcMessage>,
        on_ipc: fn(&Sender<IpcMessage>, &str),
    ) -> Result<Self, String> {
        let web_context = WebContext::default().ok_or("no default WebKit context")?;

        if let Some(Scheme { name, resolve }) = content.scheme {
            // Without this the scheme is an insecure origin and the page loses
            // every API gated on a secure context.
            web_context
                .security_manager()
                .ok_or("no WebKit security manager")?
                .register_uri_scheme_as_secure(&name);
            // The resolver moves into the handler; both live as long as the
            // process does.
            web_context.register_uri_scheme(&name, move |request| serve(request, &resolve));
        }

        let ucm = UserContentManager::new();
        for source in [BRIDGE_SCRIPT, init_script] {
            ucm.add_script(&UserScript::new(
                source,
                UserContentInjectedFrames::TopFrame,
                UserScriptInjectionTime::Start,
                &[],
                &[],
            ));
        }
        if !ucm.register_script_message_handler(IPC_HANDLER) {
            return Err("failed to register the `ipc` script-message handler".into());
        }
        ucm.connect_script_message_received(Some(IPC_HANDLER), move |_, result| {
            if let Some(body) = result.js_value().map(|v| v.to_string()) {
                on_ipc(&ipc_tx, &body);
            }
        });

        let webview: WebView =
            WebView::new_with_context_and_user_content_manager(&web_context, &ucm);
        // Paired with the window's RGBA visual below, this is what yields real
        // per-pixel alpha instead of an opaque backing.
        webview.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));

        let window = gtk::OffscreenWindow::new();
        window.set_default_size(size.width.cast_signed(), size.height.cast_signed());
        window.set_app_paintable(true);
        match WidgetExt::screen(&window).and_then(|screen| screen.rgba_visual()) {
            Some(visual) => window.set_visual(Some(&visual)),
            None => return Err("display has no RGBA visual; cannot composite the UI".into()),
        }
        window.add(&webview);
        // Realizes the widget's GdkWindow even though nothing is mapped, which
        // is what lets `gtk_main_do_event` route synthesized input to it.
        window.show_all();

        webview.load_uri(&content.entry);

        Ok(Self {
            window,
            webview,
            capture: Capture::new(size),
            input: InputState::new(),
        })
    }

    /// Evaluate `script` in the page's main world. Fire-and-forget, like wry's
    /// `evaluate_script`.
    pub fn evaluate_script(&self, script: &str) {
        self.webview
            .evaluate_javascript(script, None, None, gio::Cancellable::NONE, |_| {});
    }

    /// Resize the off-screen surface to `size` physical pixels. Idempotent
    /// when the size is unchanged.
    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if self.capture.size() == size {
            return;
        }
        self.capture.resize(size);
        let size = self.capture.size();
        self.window
            .resize(size.width.cast_signed(), size.height.cast_signed());
    }

    /// Scale CSS pixels so the page renders at the window's DPI. The offscreen
    /// window itself is always sized in physical pixels.
    pub fn set_scale_factor(&self, scale: f64) {
        self.webview.set_zoom_level(scale);
    }

    /// Allocate the overlay texture if needed, returning its view **only when
    /// it was just created**. The caller installs it on viso through
    /// [`foldit_core::App::set_overlay_texture`]; on later frames
    /// [`Self::upload`] refreshes the same texture in place.
    pub fn ensure_texture(&mut self, device: &wgpu::Device) -> Option<&wgpu::TextureView> {
        self.capture.ensure_texture(device)
    }

    /// Copy the page's latest paint into the overlay texture.
    pub fn upload(&mut self, queue: &wgpu::Queue) {
        self.capture.upload(&self.window, queue);
    }

    /// Feed a winit window event to the page.
    pub fn on_window_event(&mut self, event: &winit::event::WindowEvent) {
        input::dispatch(&self.webview, &mut self.input, event);
    }

    /// Give the page keyboard focus.
    pub fn focus(&self) {
        self.webview.grab_focus();
    }
}

/// Answer one custom-scheme request from `resolve`.
fn serve(request: &URISchemeRequest, resolve: &Resolver) {
    let path = request
        .path()
        .map_or_else(|| "/".to_owned(), |path| path.to_string());

    let Some((bytes, mime)) = resolve(&path) else {
        let mut error = glib::Error::new(gio::IOErrorEnum::NotFound, "not found");
        request.finish_error(&mut error);
        return;
    };
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
    request.finish(&stream, len, Some(&mime));
}
