//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! SoundCraft in an `<iframe>` (such as a Nextcloud app). It builds on the browser build's own
//! file handling: sessions travel as bundles (`.scraftz`), and every file SoundCraft saves or
//! exports goes through the engine's delivery hook, which here hands it to the parent instead of
//! downloading it. Messages are plain objects with a `type`:
//!
//! - `soundcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `soundcraft:open` (from the parent): `{ name, path, bytes | blob }`. A bundle (`.scraftz`)
//!   opens as it is. A session saved in a folder (`.scraft`) asks for the audio it uses with
//!   `need` first. An audio file opens as a new session of the same name with the file on a track.
//! - `soundcraft:need` (to the parent): `{ paths }`, the audio files a `.scraft` session uses, as
//!   paths on the parent's side.
//! - `soundcraft:file` (from the parent): `{ path, bytes | blob }` one of them, or
//!   `{ path, error }` when it can't be read (the session opens with that file missing).
//! - `soundcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file SoundCraft delivers: Save (`<session>.scraftz`), bounces and exports. The parent stores
//!   it and reports failures itself.
//! - `soundcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `soundcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts with an empty session instead of the demo, and the parent guards leaving the page.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::rc::Rc;

use serde_json::json;
use soundcraft_engine::io;
use soundcraft_ui_egui::SoundApp;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// What arrived from the parent, handled between frames.
enum Incoming {
    Open { name: String, path: String, bytes: Vec<u8> },
    File { path: String, bytes: Option<Vec<u8>> },
}

/// A `.scraft` session waiting for its audio: by the path the session names it with (relative to
/// the session's folder, or absolute), the parent's path asked for and the bytes once they came.
struct Pending {
    name: String,
    text: String,
    sources: BTreeMap<String, (String, Option<Vec<u8>>)>,
    waiting: usize,
}

thread_local! {
    /// The bridge, for the engine's delivery hook (a plain `fn`).
    static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    incoming: Rc<RefCell<VecDeque<Incoming>>>,
    pending: Rc<RefCell<Option<Pending>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, incoming: Rc::default(), pending: Rc::default(), reported: Rc::default() })
    }

    /// Hand what SoundCraft saves and exports to the parent instead of downloading it. Call it
    /// before anything else installs the engine's delivery hook (it can be set only once).
    pub fn install(&self) {
        BRIDGE.with(|b| *b.borrow_mut() = Some(self.clone()));
        io::set_deliver(deliver);
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "soundcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "soundcraft:failed".into()), ("error", error.into())]);
    }

    /// Before each frame: open what arrived, and tell the parent when unsaved changes appear or
    /// go away.
    pub fn sync(&self, app: &mut SoundApp) {
        loop {
            let next = self.incoming.borrow_mut().pop_front();
            let Some(next) = next else { break };
            let result = match next {
                Incoming::Open { name, path, bytes } => self.open(app, &name, &path, bytes),
                Incoming::File { path, bytes } => self.arrived(app, &path, bytes),
            };
            if let Err(e) = result {
                log::error!("{e}");
                app.ui.status = e;
            }
        }
        let dirty = app.engine.is_dirty();
        if self.reported.get() != Some(dirty) && self.send(&[("type", "soundcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    fn open(&self, app: &mut SoundApp, name: &str, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        if has_extension(name, io::BUNDLE_EXTENSION) {
            let _ = app.run("transport.stop", json!({}));
            let missing = io::open_session_bytes(&mut app.engine, name, &bytes).map_err(|e| e.to_string())?;
            app.engine.mark_clean();
            app.ui.status = opened(name, missing.len());
            Ok(())
        } else if has_extension(name, soundcraft_model::SESSION_EXTENSION) {
            self.begin_session(app, name, path, bytes)
        } else {
            open_audio(app, name, &bytes)
        }
    }

    /// A session saved in a folder: ask the parent for its audio first.
    fn begin_session(&self, app: &mut SoundApp, name: &str, path: &str, bytes: Vec<u8>) -> Result<(), String> {
        let text = String::from_utf8(bytes).map_err(|_| format!("{name} is not a SoundCraft session"))?;
        let session = soundcraft_model::Session::from_json(&text).map_err(|e| format!("{name}: {e}"))?;
        let dir = Path::new(path).parent().filter(|_| path.starts_with('/')).unwrap_or_else(|| Path::new("/"));
        let sources: BTreeMap<String, (String, Option<Vec<u8>>)> = session
            .sources
            .iter()
            .map(|s| {
                let on_parent = if Path::new(&s.path).is_absolute() { s.path.clone() } else { dir.join(&s.path).to_string_lossy().into_owned() };
                (s.path.clone(), (on_parent, None))
            })
            .collect();
        let pending = Pending { name: name.to_string(), text, waiting: sources.len(), sources };
        if pending.waiting == 0 {
            return finish_session(app, pending);
        }
        let paths = js_sys::Array::new();
        for (on_parent, _) in pending.sources.values() {
            paths.push(&JsValue::from_str(on_parent));
        }
        self.send(&[("type", "soundcraft:need".into()), ("paths", paths.into())]);
        *self.pending.borrow_mut() = Some(pending);
        app.ui.status = "Loading the session's audio…".into();
        Ok(())
    }

    /// One of the session's audio files (or the news that it can't be read).
    fn arrived(&self, app: &mut SoundApp, path: &str, bytes: Option<Vec<u8>>) -> Result<(), String> {
        let done = {
            let mut pending = self.pending.borrow_mut();
            let Some(p) = pending.as_mut() else { return Ok(()) };
            let mut matched = false;
            for (on_parent, slot) in p.sources.values_mut() {
                if on_parent == path && slot.is_none() {
                    *slot = Some(bytes.clone().unwrap_or_default());
                    matched = true;
                }
            }
            if !matched {
                return Ok(());
            }
            p.waiting = p.sources.values().filter(|(_, slot)| slot.is_none()).count();
            if p.waiting > 0 {
                return Ok(());
            }
            pending.take()
        };
        match done {
            Some(p) => finish_session(app, p),
            None => Ok(()),
        }
    }

    fn post_save(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", "soundcraft:save".into()), ("name", name.into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    /// Hear files from the parent.
    pub fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let incoming = self.incoming.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            let kind = string(&data, "type");
            let path = string(&data, "path").unwrap_or_default();
            let (incoming, ctx) = (incoming.clone(), ctx.clone());
            match kind.as_deref() {
                Some("soundcraft:open") => {
                    let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Audio.wav".to_string());
                    wasm_bindgen_futures::spawn_local(async move {
                        match bytes(&data).await {
                            Some(bytes) => incoming.borrow_mut().push_back(Incoming::Open { name, path, bytes }),
                            None => log::error!("couldn't read {name} from the page"),
                        }
                        ctx.request_repaint();
                    });
                }
                Some("soundcraft:file") => {
                    wasm_bindgen_futures::spawn_local(async move {
                        let bytes = bytes(&data).await;
                        if bytes.is_none() {
                            log::warn!("{path}: {}", string(&data, "error").unwrap_or_else(|| "not available".into()));
                        }
                        incoming.borrow_mut().push_back(Incoming::File { path, bytes });
                        ctx.request_repaint();
                    });
                }
                _ => {}
            }
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// The engine's delivery hook in host mode: the finished file goes to the parent.
fn deliver(name: &str, bytes: &[u8]) -> Result<(), String> {
    BRIDGE.with(|b| match b.borrow().as_ref() {
        Some(bridge) => bridge.post_save(name, bytes),
        None => Err("no page to hand the file to".into()),
    })
}

/// An audio file opens as a new session named after it, with the file on a track. Save stores
/// that session as a bundle (`<name>.scraftz`).
fn open_audio(app: &mut SoundApp, name: &str, bytes: &[u8]) -> Result<(), String> {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let info = soundcraft_audio_io::probe(bytes, ext.as_deref()).map_err(|e| format!("{name}: {e}"))?;
    let rate = Some(info.sample_rate).filter(|r| soundcraft_time::SampleRate::COMMON.contains(r)).unwrap_or(48_000);
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Untitled").to_string();
    let _ = app.run("transport.stop", json!({}));
    app.run("session.new", json!({"name": stem, "sample_rate": rate}))?;
    io::import_audio_bytes(&mut app.engine, name, bytes, None, None, 0).map_err(|e| e.to_string())?;
    app.engine.path = Some(format!("{}.{}", io::sanitize_name(&stem), io::BUNDLE_EXTENSION));
    app.engine.mark_clean();
    app.ui.status = format!("Opened {name}");
    Ok(())
}

/// Open a `.scraft` session with the audio that came: packed with it into a bundle, the way the
/// browser build opens sessions.
fn finish_session(app: &mut SoundApp, p: Pending) -> Result<(), String> {
    use std::io::Write as _;
    let zerr = |e: zip::result::ZipError| format!("{}: {e}", p.name);
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    let stem = Path::new(&p.name).file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled");
    w.start_file(format!("{}.{}", io::sanitize_name(stem), soundcraft_model::SESSION_EXTENSION), opts).map_err(zerr)?;
    w.write_all(p.text.as_bytes()).map_err(|e| e.to_string())?;
    for (in_session, (_, bytes)) in &p.sources {
        // A file the parent couldn't read stays out: the session opens with it missing.
        if let Some(bytes) = bytes.as_ref().filter(|b| !b.is_empty()) {
            w.start_file(in_session.as_str(), opts).map_err(zerr)?;
            w.write_all(bytes).map_err(|e| e.to_string())?;
        }
    }
    let bundle = w.finish().map_err(zerr)?.into_inner();
    let _ = app.run("transport.stop", json!({}));
    let missing = io::open_session_bytes(&mut app.engine, &p.name, &bundle).map_err(|e| e.to_string())?;
    app.engine.mark_clean();
    app.ui.status = opened(&p.name, missing.len());
    Ok(())
}

fn opened(name: &str, missing: usize) -> String {
    if missing == 0 { format!("Opened {name}") } else { format!("Opened {name} ({missing} missing media)") }
}

fn has_extension(name: &str, ext: &str) -> bool {
    Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

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

/// `bytes` (an `ArrayBuffer` or `Uint8Array`) or `blob`.
async fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        return Some(array.to_vec());
    }
    if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        return Some(js_sys::Uint8Array::new(&bytes).to_vec());
    }
    let blob = js_sys::Reflect::get(data, &JsValue::from_str("blob")).ok()?.dyn_into::<web_sys::Blob>().ok()?;
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await.ok()?;
    Some(js_sys::Uint8Array::new(&buffer).to_vec())
}
