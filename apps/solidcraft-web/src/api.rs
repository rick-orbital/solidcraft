//! Promise-based access to the running app's existing control channel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use serde_json::{Value, json};
use solidcraft_ui_egui::ControlRequest;
use wasm_bindgen::{JsValue, closure::Closure};

const MAX_PENDING: usize = 256;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

struct Pending {
    reply: Receiver<Value>,
    resolve: js_sys::Function,
    reject: js_sys::Function,
    load_path: Option<String>,
}

#[derive(Clone)]
pub struct Api {
    sender: Sender<ControlRequest>,
    pending: Rc<RefCell<Vec<Pending>>>,
    ctx: egui::Context,
    snapshots: Rc<RefCell<Vec<(String, js_sys::Function, js_sys::Function)>>>,
    input_blocked: Rc<Cell<bool>>,
    lock_ops: Rc<RefCell<Vec<(String, Value, js_sys::Function, js_sys::Function)>>>,
    lock_owner: Rc<RefCell<Option<String>>>,
    step_owner: Rc<RefCell<Option<String>>>,
    load_active: Rc<Cell<bool>>,
    next_load: Rc<Cell<u32>>,
    loaded_path: Rc<RefCell<Option<String>>>,
    subscriber: Rc<RefCell<Option<js_sys::Function>>>,
    last_generation: Rc<RefCell<Option<(String, u64)>>>,
}

fn params(value: &JsValue) -> Result<Value, String> {
    if value.is_undefined() || value.is_null() {
        return Ok(json!({}));
    }
    let text = js_sys::JSON::stringify(value).ok().and_then(|v| v.as_string()).ok_or("params must be JSON serializable")?;
    if text.len() > MAX_JSON_BYTES {
        return Err("params exceed 16 MiB".into());
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid params: {e}"))
}

fn error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

impl Api {
    pub fn install(sender: Sender<ControlRequest>, ctx: egui::Context, expose_debug: bool) -> Result<Self, JsValue> {
        let api = Self {
            sender,
            pending: Rc::default(),
            ctx,
            snapshots: Rc::default(),
            input_blocked: Rc::default(),
            lock_ops: Rc::default(),
            lock_owner: Rc::default(),
            step_owner: Rc::default(),
            load_active: Rc::default(),
            next_load: Rc::default(),
            loaded_path: Rc::default(),
            subscriber: Rc::default(),
            last_generation: Rc::default(),
        };
        if !expose_debug {
            return Ok(api);
        }
        let object = js_sys::Object::new();
        let request = api.clone();
        let execute = api.clone();
        let commands = api.clone();
        let inspect = api.clone();
        for (name, function) in [
            (
                "request",
                Closure::<dyn Fn(String, JsValue) -> js_sys::Promise>::new(move |method: String, value: JsValue| {
                    request.request(&method, params(&value))
                })
                .into_js_value(),
            ),
            (
                "execute",
                Closure::<dyn Fn(String, JsValue) -> js_sys::Promise>::new(move |command: String, value: JsValue| {
                    execute.request("engine.execute", params(&value).map(|p| json!({"command": command, "params": p})))
                })
                .into_js_value(),
            ),
            ("commands", Closure::<dyn Fn() -> js_sys::Promise>::new(move || commands.request("engine.commands", Ok(json!({})))).into_js_value()),
            (
                "inspect",
                Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |value: JsValue| inspect.request("document.inspect", params(&value)))
                    .into_js_value(),
            ),
        ] {
            js_sys::Reflect::set(&object, &name.into(), &function)?;
        }
        let snapshot = api.clone();
        let f = Closure::<dyn Fn(String) -> js_sys::Promise>::new(move |token: String| {
            if snapshot.lock_owner.borrow().as_deref() != Some(token.as_str()) {
                return js_sys::Promise::reject(&error("lock owner required"));
            }
            if !snapshot.snapshots.borrow().is_empty() {
                return js_sys::Promise::reject(&error("snapshot already pending"));
            }
            js_sys::Promise::new(&mut |resolve, reject| {
                snapshot.snapshots.borrow_mut().push((token.clone(), resolve, reject));
                snapshot.ctx.request_repaint();
            })
        });
        js_sys::Reflect::set(&object, &"probeSnapshot".into(), &f.into_js_value())?;
        let load = api.clone();
        let f = Closure::<dyn Fn(js_sys::Uint8Array) -> js_sys::Promise>::new(move |bytes: js_sys::Uint8Array| {
            if load.load_active.get() {
                return js_sys::Promise::reject(&error("load already pending"));
            }
            if bytes.length() as usize > solidcraft_engine::io::MAX_DESIGN_BYTES {
                return js_sys::Promise::reject(&error("design exceeds byte limit"));
            }
            let bytes = bytes.to_vec();
            match solidcraft_engine::io::read_design(&bytes) {
                Ok(doc) if !doc.links.is_empty() => return js_sys::Promise::reject(&error("external design links unsupported in managed probe")),
                Err(e) => return js_sys::Promise::reject(&error(&e.to_string())),
                _ => {}
            }
            let id = load.next_load.get().wrapping_add(1);
            load.next_load.set(id);
            let path = format!("/probe/design-{id}.solidcraft");
            solidcraft_engine::io::vfs::insert(&path, bytes);
            load.request_kind("engine.execute", Ok(json!({"command":"doc.open", "params":{"path":path, "_probe_preserve_name":true}})), Some(path))
        });
        js_sys::Reflect::set(&object, &"probeLoad".into(), &f.into_js_value())?;
        let gate = api.input_blocked.clone();
        let f = Closure::<dyn Fn(bool)>::new(move |blocked| gate.set(blocked));
        js_sys::Reflect::set(&object, &"probeBlockInput".into(), &f.into_js_value())?;
        for name in [
            "probeLock",
            "probeUnlock",
            "probeLockedParameter",
            "probeGeneration",
            "probeManaged",
            "probeQueueOpened",
            "probeFileQueue",
            "probeLockedStep",
            "probePendingDownloads",
        ] {
            let host = api.clone();
            let action = name.to_string();
            let f = Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |value: JsValue| {
                let value = match params(&value) {
                    Ok(value) => value,
                    Err(e) => return js_sys::Promise::reject(&error(&e)),
                };
                if !host.lock_ops.borrow().is_empty() {
                    return js_sys::Promise::reject(&error("probe operation pending"));
                }
                js_sys::Promise::new(&mut |resolve, reject| {
                    host.lock_ops.borrow_mut().push((action.clone(), value.clone(), resolve, reject));
                    host.ctx.request_repaint();
                })
            });
            js_sys::Reflect::set(&object, &name.into(), &f.into_js_value())?;
        }
        let seed = api.clone();
        let f = Closure::<dyn Fn(js_sys::Uint8Array, String) -> Result<(), JsValue>>::new(move |bytes: js_sys::Uint8Array, token: String| {
            if seed.lock_owner.borrow().as_deref() != Some(token.as_str()) {
                return Err(error("lock owner required"));
            }
            if bytes.length() as usize > 16 * 1024 * 1024 {
                return Err(error("STEP exceeds probe byte limit"));
            }
            solidcraft_engine::io::vfs::insert("/probe/import.step", bytes.to_vec());
            *seed.step_owner.borrow_mut() = Some(token);
            Ok(())
        });
        js_sys::Reflect::set(&object, &"probeSeedStep".into(), &f.into_js_value())?;
        let watch = api.clone();
        let f = Closure::<dyn Fn(js_sys::Function)>::new(move |callback: js_sys::Function| {
            *watch.subscriber.borrow_mut() = Some(callback);
            *watch.last_generation.borrow_mut() = None;
            watch.ctx.request_repaint();
        });
        js_sys::Reflect::set(&object, &"probeSubscribe".into(), &f.into_js_value())?;
        let unwatch = api.clone();
        let f = Closure::<dyn Fn()>::new(move || {
            *unwatch.subscriber.borrow_mut() = None;
            *unwatch.last_generation.borrow_mut() = None;
        });
        js_sys::Reflect::set(&object, &"probeUnsubscribe".into(), &f.into_js_value())?;
        let window = web_sys::window().ok_or_else(|| error("no window"))?;
        js_sys::Reflect::set(&window, &"solidcraft".into(), &object)?;
        Ok(api)
    }

    fn request(&self, method: &str, params: Result<Value, String>) -> js_sys::Promise {
        self.request_kind(method, params, None)
    }

    fn request_kind(&self, method: &str, params: Result<Value, String>, load_path: Option<String>) -> js_sys::Promise {
        let params = match params {
            Ok(params) => params,
            Err(message) => return js_sys::Promise::reject(&error(&message)),
        };
        if self.pending.borrow().len() >= MAX_PENDING {
            if let Some(path) = &load_path {
                let _ = solidcraft_engine::io::vfs::remove(path);
            }
            return js_sys::Promise::reject(&error("too many pending SolidCraft requests (maximum 256)"));
        }
        if load_path.is_some() {
            self.load_active.set(true);
        }
        js_sys::Promise::new(&mut |resolve, reject| {
            let (request, reply) = ControlRequest::new(method, params.clone());
            if self.sender.send(request).is_err() {
                if let Some(path) = &load_path {
                    self.load_active.set(false);
                    let _ = solidcraft_engine::io::vfs::remove(path);
                }
                let _ = reject.call1(&JsValue::UNDEFINED, &error("SolidCraft is not running"));
                return;
            }
            self.pending.borrow_mut().push(Pending { reply, resolve, reject, load_path: load_path.clone() });
            self.ctx.request_repaint();
        })
    }

    pub fn poll_lock(&self, app: &mut solidcraft_ui_egui::SolveApp, ctx: &egui::Context, files: &crate::files::Files) {
        let requests = std::mem::take(&mut *self.lock_ops.borrow_mut());
        for (action, value, resolve, reject) in requests {
            let token = value.get("token").and_then(Value::as_str).filter(|t| t.len() >= 16 && t.len() <= 256);
            let owns = token.is_some_and(|t| self.lock_owner.borrow().as_deref() == Some(t));
            let result = match action.as_str() {
                "probeLock" if !self.pending.borrow().is_empty() => Err("control request pending".into()),
                "probeLock" if token.is_none() => Err("owner token required".into()),
                "probeLock" => match app.probe_lock(ctx) {
                    Ok(()) => {
                        *self.lock_owner.borrow_mut() = token.map(str::to_string);
                        Ok(Value::Null)
                    }
                    Err(e) => Err(e.to_string()),
                },
                "probeManaged" if !self.pending.borrow().is_empty() => Err("control request pending".into()),
                "probeManaged" => app.probe_enter_managed(ctx).map(|_| Value::Null).map_err(str::to_string),
                "probeUnlock" if owns => {
                    app.probe_unlock();
                    *self.lock_owner.borrow_mut() = None;
                    *self.step_owner.borrow_mut() = None;
                    let _ = solidcraft_engine::io::vfs::remove("/probe/import.step");
                    Ok(Value::Null)
                }
                "probeUnlock" => Err("lock owner required".into()),
                "probeGeneration" => Ok(json!({"generation":app.session.probe_generation.to_string(), "document_id":app.session.document_id})),
                "probeQueueOpened" => {
                    files.opened.borrow_mut().push("/probe/design.solidcraft".into());
                    Ok(Value::Null)
                }
                "probeFileQueue" => Ok(json!({"opened":files.opened.borrow().len()})),
                "probePendingDownloads" => Ok(json!({"pending":solidcraft_engine::io::vfs::pending_writes_prefix(crate::files::DOWNLOADS)})),
                "probeLockedStep" if owns && token == self.step_owner.borrow().as_deref() => {
                    let result = app.probe_locked_step();
                    *self.step_owner.borrow_mut() = None;
                    let _ = solidcraft_engine::io::vfs::remove("/probe/import.step");
                    result
                }
                "probeLockedStep" if owns => Err("fresh STEP seed required".into()),
                "probeLockedStep" => Err("lock owner required".into()),
                "probeLockedParameter" if owns => app.probe_locked_parameter(value.get("params").cloned().unwrap_or(Value::Null)),
                "probeLockedParameter" => Err("lock owner required".into()),
                _ => Err("unknown probe operation".into()),
            };
            match result {
                Ok(value) => {
                    let _ = resolve.call1(&JsValue::UNDEFINED, &js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL));
                }
                Err(e) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &error(&e));
                }
            }
        }
    }

    /// Probe notification: frame-coalesced, carrying the latest cumulative generation.
    pub fn poll_generation(&self, session: &solidcraft_engine::Session) {
        let current = (session.document_id.clone(), session.probe_generation);
        if self.last_generation.borrow().as_ref() == Some(&current) {
            return;
        }
        *self.last_generation.borrow_mut() = Some(current.clone());
        let callback = self.subscriber.borrow().clone();
        if let Some(callback) = callback {
            let value = json!({"document_id":current.0,"generation":current.1.to_string()});
            let delivered = js_sys::JSON::parse(&value.to_string()).and_then(|value| callback.call1(&JsValue::UNDEFINED, &value));
            if delivered.is_err() {
                *self.subscriber.borrow_mut() = None;
                *self.last_generation.borrow_mut() = None;
            }
        }
    }

    pub fn input_blocked(&self) -> bool {
        self.input_blocked.get()
    }

    /// Gate 0 probe only: serialize on the app frame; not a stable mutation boundary.
    pub fn poll_snapshot(&self, session: &solidcraft_engine::Session) {
        let requests = std::mem::take(&mut *self.snapshots.borrow_mut());
        if requests.is_empty() {
            return;
        }
        for (token, resolve, reject) in requests {
            if !session.probe_locked || self.lock_owner.borrow().as_deref() != Some(token.as_str()) {
                let _ = reject.call1(&JsValue::UNDEFINED, &error("lock owner required"));
                continue;
            }
            let design = session.design();
            let result = if design.links.is_empty() {
                solidcraft_engine::io::write_design_container(&design).map_err(|e| e.to_string())
            } else {
                Err("external design links unsupported in managed probe".to_string())
            };
            match result {
                Ok(bytes) => {
                    let object = js_sys::Object::new();
                    let set = js_sys::Reflect::set(&object, &"bytes".into(), &js_sys::Uint8Array::from(bytes.as_slice()))
                        .and_then(|_| js_sys::Reflect::set(&object, &"generation".into(), &JsValue::from_str(&session.probe_generation.to_string())))
                        .and_then(|_| js_sys::Reflect::set(&object, &"document_id".into(), &JsValue::from_str(&session.document_id)));
                    if set.is_ok() {
                        let _ = resolve.call1(&JsValue::UNDEFINED, &object);
                    } else {
                        let _ = reject.call1(&JsValue::UNDEFINED, &error("snapshot envelope failed"));
                    }
                }
                Err(e) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &error(&e));
                }
            }
        }
    }

    pub fn poll(&self) {
        let mut completed = Vec::new();
        self.pending.borrow_mut().retain_mut(|pending| {
            let result = match pending.reply.try_recv() {
                Ok(reply) => {
                    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
                        js_sys::JSON::parse(&reply.get("result").unwrap_or(&Value::Null).to_string())
                    } else {
                        Err(error(reply.get("error").and_then(Value::as_str).unwrap_or("SolidCraft request failed")))
                    }
                }
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(error("SolidCraft request dropped")),
            };
            if let Some(path) = &pending.load_path {
                self.load_active.set(false);
                if result.is_ok() {
                    if let Some(old) = self.loaded_path.borrow_mut().replace(path.clone()) {
                        let _ = solidcraft_engine::io::vfs::remove(&old);
                    }
                } else {
                    let _ = solidcraft_engine::io::vfs::remove(path);
                }
            }
            completed.push((pending.resolve.clone(), pending.reject.clone(), result));
            false
        });
        // Release the queue before calling JavaScript, which can submit another request.
        for (resolve, reject, result) in completed {
            match result {
                Ok(value) => {
                    let _ = resolve.call1(&JsValue::UNDEFINED, &value);
                }
                Err(error) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &error);
                }
            }
        }
        if !self.pending.borrow().is_empty() {
            self.ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
}
