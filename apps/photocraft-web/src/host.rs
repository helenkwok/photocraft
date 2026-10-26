//! Host modes: a page that runs PhotoCraft (as an `<iframe>`, or in its own document) opens
//! documents in it and stores what it saves instead of the browser downloading. The same
//! messages as WordCraft's, GridCraft's and PdfCraft's bridges.
//!
//! ## `?host=parent` — an `<iframe>` (such as a Nextcloud app)
//!
//! The framing page and PhotoCraft talk over `postMessage` between same-origin windows. Messages
//! are plain objects with a `type`:
//!
//! - `photocraft:ready` (to the parent): `{ version }`, sent once the app is running and
//!   listening; send a file now.
//! - `photocraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a document picked with File › Open or dropped on the window
//!   (brushes, gradients, layer styles and swatches go to their libraries, as a drop does). The
//!   name is the file's own, as a drop carries it; a payload over 512 MiB
//!   (`hostmode::MAX_OPEN_BYTES`) is refused before it is copied, and a malformed one is ignored.
//! - `photocraft:save` (to the parent): `{ name, saveAs, bytes }` (a transferred `ArrayBuffer`)
//!   for every file PhotoCraft would otherwise download — Save, Save As, Save a Copy, exports,
//!   Save for Web. The parent stores it and reports failures itself. `saveAs` says whether the
//!   file became the document's file (`hostmode::save_as_flag` decides, from what the app did
//!   with it once the frame is done):
//!   - Save of the file the host opened, under its own name: `false`. It writes the opened file
//!     as the desktop app does.
//!   - Save As to a new name, and a Save of a document the host never sent (an untitled
//!     document, or an import that changes format, like a .pdn suggested as .pcraft or a .kra as
//!     .ora): `true`. The document is now that file; the next Save sends its name with `false`.
//!   - Copies and exports that don't become the document (Save a Copy, Save As to a format that
//!     can't hold the layers, rendered images, PDFs, .xmp sidecars, exported measurements):
//!     `false`; the document's file stays the same. So does a write over a file one of the
//!     app's documents already had, host-sent or not: it replaces that file.
//! - `photocraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//!   A save handed to the host counts as written, so it clears the state.
//! - `photocraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! Only the parent window on the page's own origin is heard or answered.
//!
//! ## `data-host="page"` — the page's own document
//!
//! A page that loads the module into its own document (its own `<canvas id="photocraft_canvas"
//! data-host="page">`, no iframe) gets the same behaviour over DOM `CustomEvent`s on the canvas
//! instead of `postMessage`: the same four events outward (`ready`, `save`, `dirty`, `failed`,
//! with the message fields as the event's `detail`), and the page dispatches `photocraft:open` on
//! the canvas with `detail: { name, bytes }` to open a document. Events don't bubble, so two
//! editors on one page can't hear each other, and the canvas is the only target PhotoCraft
//! listens on.
//!
//! In both host modes the host sends the document to edit, so the app starts on its empty
//! workspace with no sample, saves are handed to the host rather than downloaded, and the host
//! guards leaving the page (the browser's own guard is skipped). PhotoCraft has no user identity
//! of its own to name, so there is no `?author=`.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use photocraft_ui_egui::{Inbox, PhotocraftApp, Services};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

use crate::hostmode::{MAX_OPEN_BYTES, file_name, open_allowed, save_as_flag};

/// How a host page is reached. The modes behave the same; they differ only in how an event goes
/// out ([`Transport::send`]) and how an open request comes in ([`Bridge::listen`]).
#[derive(Clone)]
enum Transport {
    /// `?host=parent`: `postMessage` between same-origin windows.
    Parent { window: web_sys::Window, parent: web_sys::Window, origin: String },
    /// `data-host="page"`: DOM `CustomEvent`s on the editor's own canvas.
    Page { canvas: web_sys::HtmlCanvasElement },
}

impl Transport {
    /// Hand an event to the host. In parent mode this is a `postMessage` whose object carries a
    /// `type` field; in page mode a `CustomEvent` of that name on the canvas, whose `detail`
    /// holds the fields and which never bubbles (two editors on one page must not hear each
    /// other). `transfer` hands an `ArrayBuffer` to the parent instead of copying it (parent
    /// mode only; the page's document already shares the buffer's bytes).
    fn send(&self, event: &str, fields: &[(&str, JsValue)], transfer: Option<&js_sys::ArrayBuffer>) -> Result<(), String> {
        match self {
            Transport::Parent { parent, origin, .. } => {
                let mut message_fields = Vec::with_capacity(fields.len() + 1);
                message_fields.push(("type", JsValue::from_str(event)));
                message_fields.extend_from_slice(fields);
                let message = message(&message_fields).ok_or_else(|| "couldn't build the message".to_string())?;
                let sent = match transfer {
                    Some(buffer) => parent.post_message_with_transfer(&message, origin, &js_sys::Array::of1(buffer)),
                    None => parent.post_message(&message, origin),
                };
                sent.map_err(|e| js_error(&e, "couldn't post the message"))
            }
            Transport::Page { canvas } => {
                let detail = message(fields).ok_or_else(|| "couldn't build the event detail".to_string())?;
                let init = web_sys::CustomEventInit::new();
                // The builder methods (`bubbles(..)`, `detail(..)`) are deprecated in web-sys
                // 0.3.106 and fail clippy -D warnings; use the setters.
                init.set_bubbles(false);
                init.set_detail(&detail);
                let custom = web_sys::CustomEvent::new_with_event_init_dict(event, &init).map_err(|e| js_error(&e, "couldn't create the event"))?;
                // A listener calling `preventDefault` would be the bool; not a send failure.
                canvas.dispatch_event(&custom).map_err(|e| js_error(&e, "couldn't dispatch the event"))?;
                Ok(())
            }
        }
    }
}

#[derive(Clone)]
pub struct Bridge {
    transport: Transport,
    /// Files the app wrote since the last sync, waiting to go to the host with their `saveAs`
    /// decided from what they became.
    pending: Rc<RefCell<Vec<(String, Vec<u8>)>>>,
    /// The files that were documents' files at the last sync ([`Bridge::sync`]): what a written
    /// file must not count as making new.
    bound: Rc<RefCell<HashSet<String>>>,
    /// File names the host sent (`photocraft:open`): a save under one of them writes the host's
    /// own file, not a new one.
    opened: Rc<RefCell<HashSet<String>>>,
    /// The unsaved-changes state last reported to the host.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    fn over(transport: Transport) -> Self {
        Self { transport, pending: Rc::default(), bound: Rc::default(), opened: Rc::default(), reported: Rc::default() }
    }

    /// The parent bridge for `?host=parent`. `None` when the page isn't framed (there is no
    /// parent to talk to), so opening that URL directly stays the normal app with downloads, and
    /// when the parent is on another origin: messages only go to this origin, so the browser
    /// would drop every save without an error while the document showed as saved.
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        // An opaque origin (a `file://` page in some browsers) can't be a `postMessage` target.
        if origin == "null" {
            return None;
        }
        // Reading another origin's location throws, which is `None` too.
        if parent.location().origin().ok()? != origin {
            return None;
        }
        Some(Self::over(Transport::Parent { window, parent, origin }))
    }

    /// The page bridge for `data-host="page"`: the host page runs PhotoCraft in its own document
    /// and hears the editor's DOM `CustomEvent`s on this canvas.
    pub fn for_page(canvas: &web_sys::HtmlCanvasElement) -> Self {
        Self::over(Transport::Page { canvas: canvas.clone() })
    }

    /// Hand saved and exported files to the host instead of downloading them, and hear the files
    /// it sends. Every file the app would otherwise download now waits for the frame's end,
    /// when the app has said what became of it ([`Bridge::sync`]); nothing reaches the browser's
    /// download while a bridge is active.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.write = Some(Box::new(move |path: &str, bytes: &[u8]| {
            bridge.pending.borrow_mut().push((file_name(path), bytes.to_vec()));
            Ok(())
        }));
        // The host keeps the file once the message is handed over, so the save counts as
        // written; the web build normally leaves a browser download unconfirmed (#4962).
        services.write_unconfirmed = false;
    }

    pub fn post_ready(&self) {
        let _ = self.transport.send("photocraft:ready", &[("version", env!("CARGO_PKG_VERSION").into())], None);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        let _ = self.transport.send("photocraft:failed", &[("error", error.into())], None);
    }

    /// After each frame: hand the files the app wrote to the host, with `saveAs` decided from
    /// what they became, and tell it when unsaved changes appear or go away. The shell calls
    /// this after the app's logic and UI passes, so a save's file and its cleared dirty state
    /// reach the host together.
    pub fn sync(&self, app: &PhotocraftApp) {
        let written = std::mem::take(&mut *self.pending.borrow_mut());
        let documents = app.session.documents();
        for (name, bytes) in written {
            let before = self.bound.borrow().contains(&name);
            let after = documents.iter().any(|d| d.path.as_deref().is_some_and(|p| file_name(p) == name));
            let save_as = save_as_flag(before, after, self.opened.borrow().contains(&name));
            if let Err(e) = self.post_save(&name, &bytes, save_as) {
                log::error!("couldn't hand {name} to the host: {e}");
            }
        }
        *self.bound.borrow_mut() = documents.iter().filter_map(|d| d.path.as_deref().map(file_name)).collect();
        let dirty = app.has_unsaved_work();
        if self.reported.get() != Some(dirty) && self.transport.send("photocraft:dirty", &[("dirty", dirty.into())], None).is_ok() {
            self.reported.set(Some(dirty));
        }
    }

    fn post_save(&self, name: &str, bytes: &[u8], save_as: bool) -> Result<(), String> {
        // `photocraft:save` carries the bytes as an `ArrayBuffer`; the parent bridge transfers it.
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let fields: [(&str, JsValue); 3] = [("name", name.into()), ("saveAs", save_as.into()), ("bytes", buffer.clone().into())];
        self.transport.send("photocraft:save", &fields, Some(&buffer))
    }

    /// Hear `photocraft:open` — from the same-origin parent (a `message` on the window) or from
    /// the host document (a `photocraft:open` event on the canvas) — and queue the file like a
    /// dropped one.
    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        match &self.transport {
            Transport::Parent { window, parent, origin } => {
                let parent = parent.clone();
                let origin = origin.clone();
                let opened = self.opened.clone();
                let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                    if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                        return;
                    }
                    let data = event.data();
                    if string(&data, "type").as_deref() != Some("photocraft:open") {
                        return;
                    }
                    deliver_open(&inbox, &ctx, &data, &opened);
                });
                if window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
                    // The listener lives as long as the page.
                    on_message.forget();
                }
            }
            Transport::Page { canvas } => {
                let opened = self.opened.clone();
                let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                    // A plain `Event` of the same name carries no detail; only a `CustomEvent`.
                    let Some(open) = event.dyn_ref::<web_sys::CustomEvent>() else { return };
                    deliver_open(&inbox, &ctx, &open.detail(), &opened);
                });
                if canvas.add_event_listener_with_callback("photocraft:open", on_open.as_ref().unchecked_ref()).is_ok() {
                    // The listener lives as long as the page.
                    on_open.forget();
                }
            }
        }
    }
}

/// The file an open message or event carries, pushed onto the inbox for the next frame, like a
/// dropped file. Its name is remembered as one of the host's own files, so a later save under it
/// is `saveAs: false` ([`Bridge::sync`]).
fn deliver_open(inbox: &Inbox, ctx: &egui::Context, data: &JsValue, opened: &Rc<RefCell<HashSet<String>>>) {
    let Some(bytes) = payload_bytes(data) else { return };
    let name = file_name(&string(data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "image.png".to_string()));
    opened.borrow_mut().insert(name.clone());
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
    ctx.request_repaint();
}

/// The `bytes` an open message or event carries, as an owned buffer: an `ArrayBuffer` or
/// `Uint8Array`, and at most [`MAX_OPEN_BYTES`] — refused before it is copied. Anything else is
/// `None`, and the open is ignored.
fn payload_bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    let len = if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        array.byte_length()
    } else if let Some(buffer) = bytes.dyn_ref::<js_sys::ArrayBuffer>() {
        buffer.byte_length()
    } else {
        return None;
    };
    if !open_allowed(len as usize) {
        log::warn!("the host's open payload is larger than {} MiB; ignored", MAX_OPEN_BYTES >> 20);
        return None;
    }
    if let Some(buffer) = bytes.dyn_ref::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(buffer).to_vec())
    } else {
        Some(bytes.dyn_ref::<js_sys::Uint8Array>()?.to_vec())
    }
}

/// A message object or `CustomEvent` detail with the given fields, or `None` if one couldn't be
/// built (a field that isn't a valid property name, say).
fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// A `JsValue` error as a string, after `what` went wrong.
fn js_error(e: &JsValue, what: &str) -> String {
    format!("{what}: {}", e.as_string().unwrap_or_else(|| format!("{e:?}")))
}
