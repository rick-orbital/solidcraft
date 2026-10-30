//! Fail-closed managed-frame handshake. No document operations are enabled yet.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{MessageEvent, MessagePort};

use crate::managed_handshake::{MAJOR, MINOR, Policy, valid_request};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
struct Pending {
    request: Value,
    bytes: Option<Vec<u8>>,
    port: MessagePort,
    deadline: Option<f64>,
}
type Listener = Closure<dyn FnMut(MessageEvent)>;

/// Retains listeners and the accepted port for the lifetime of the editor frame.
pub struct Bridge {
    listener: Rc<RefCell<Option<Listener>>>,
    port_listener: Rc<RefCell<Option<Listener>>>,
    port: Rc<RefCell<Option<MessagePort>>>,
    alive: Rc<Cell<bool>>,
    pending: Rc<RefCell<Option<Pending>>>,
    loaded: Rc<Cell<bool>>,
    deadline: Rc<Cell<f64>>,
    expired: Rc<Cell<bool>>,
    last_generation: Rc<RefCell<Option<(String, u64)>>>,
    binding: Rc<RefCell<Option<(String, String)>>>,
    observed: RefCell<Option<(std::sync::Arc<solidcraft_engine::doc::Document>, solidcraft_engine::Visibility, u64)>>,
    generation: Cell<u64>,
}

impl Bridge {
    pub fn start(ctx: egui::Context) -> Self {
        let bridge = Self {
            listener: Rc::default(),
            port_listener: Rc::default(),
            port: Rc::default(),
            alive: Rc::new(Cell::new(true)),
            pending: Rc::default(),
            loaded: Rc::default(),
            deadline: Rc::default(),
            expired: Rc::default(),
            last_generation: Rc::default(),
            binding: Rc::default(),
            observed: RefCell::default(),
            generation: Cell::default(),
        };
        let listener = bridge.listener.clone();
        let port_listener = bridge.port_listener.clone();
        let accepted_port = bridge.port.clone();
        let alive = bridge.alive.clone();
        let pending = bridge.pending.clone();
        let deadline = bridge.deadline.clone();
        let binding = bridge.binding.clone();
        spawn_local(async move {
            if let Err(error) = install(listener, port_listener, accepted_port, alive, pending, deadline, binding, ctx).await {
                log::error!("managed bridge unavailable: {error:?}");
            }
        });
        bridge
    }
    fn send_error(&self, pending: &Pending, code: &str) {
        let response = response_envelope(&pending.request, json!({"ok":false,"error":{"code":code}}));
        if js_json(&response).and_then(|value| pending.port.post_message(&value)).is_err() {
            pending.port.close();
        }
    }
    pub fn editing(&self) -> bool {
        let now = web_sys::window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(f64::INFINITY);
        self.deadline.get() > now && !self.expired.get()
    }

    pub fn poll(&self, app: &mut solidcraft_ui_egui::SolveApp, ctx: &egui::Context) {
        if self.loaded.get() {
            let session = &app.session;
            let mut observed = self.observed.borrow_mut();
            if let Some((doc, visibility, engine_generation)) = observed.as_ref()
                && (!std::sync::Arc::ptr_eq(doc, &session.doc) || visibility != &session.visibility || *engine_generation != session.probe_generation)
            {
                if let Some(next) = self.generation.get().checked_add(1) {
                    self.generation.set(next);
                } else {
                    self.deadline.set(0.0);
                    self.expired.set(true);
                    app.probe_force_lock();
                }
            }
            *observed = Some((app.session.doc.clone(), app.session.visibility.clone(), app.session.probe_generation));
        }
        if self.deadline.get() > 0.0 && !self.editing() {
            self.deadline.set(0.0);
            self.expired.set(true);
            app.probe_force_lock();
        }
        if self.editing() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            let state = (app.session.document_id.clone(), self.generation.get());
            if self.last_generation.borrow().as_ref() != Some(&state) {
                *self.last_generation.borrow_mut() = Some(state.clone());
                if let (Some(port), Some((design, epoch))) = (self.port.borrow().as_ref(), self.binding.borrow().as_ref()) {
                    let event = json!({"version":{"major":MAJOR,"minor":MINOR},"kind":"event","requestId":"mutation","sessionEpoch":epoch,"designId":design,"payload":{"event":"mutation","documentId":state.0,"generation":state.1.to_string()}});
                    if js_json(&event).and_then(|value| port.post_message(&value)).is_err() {
                        self.deadline.set(0.0);
                        self.expired.set(true);
                        app.probe_force_lock();
                        port.close();
                    }
                }
            }
        }
        let Some(pending) = self.pending.borrow_mut().take() else { return };
        let action = pending.request["payload"]["action"].as_str().unwrap_or("");
        let result = match action {
            "capabilities" => {
                let test_writable = option_env!("CURIE_CAD_TEST_WRITABLE") == Some("1");
                let actions = if test_writable {
                    vec!["capabilities", "load", "load.empty", "snapshot", "edit.start", "edit.renew", "edit.stop"]
                } else {
                    vec!["capabilities", "load", "snapshot"]
                };
                Ok(
                    json!({"actions":actions,"maxDocumentBytes":MAX_DOCUMENT_BYTES,"writable":test_writable,"build":option_env!("CURIE_EDITOR_BUILD").unwrap_or("unverified"),"protocol":{"major":MAJOR,"minor":MINOR}}),
                )
            }
            "load" if !self.loaded.get() && app.session.probe_locked && solidcraft_ui_egui::documents::active_is_blank(app) => {
                match solidcraft_engine::io::read_design(pending.bytes.as_deref().unwrap_or_default()) {
                    Ok(doc) if doc.links.is_empty() => {
                        solidcraft_ui_egui::documents::adopt(app, solidcraft_engine::Session::new(doc));
                        app.home.open = false;
                        self.loaded.set(true);
                        Ok(json!({"documentId":app.session.document_id,"generation":self.generation.get().to_string()}))
                    }
                    _ => Err("invalid_or_unsupported_document"),
                }
            }
            "load.empty" if !self.loaded.get() && app.session.probe_locked && solidcraft_ui_egui::documents::active_is_blank(app) => {
                self.loaded.set(true);
                app.home.open = false;
                Ok(json!({"documentId":app.session.document_id,"generation":self.generation.get().to_string()}))
            }
            "load" | "load.empty" => Err("load_unavailable"),
            "edit.start"
                if option_env!("CURIE_CAD_TEST_WRITABLE") == Some("1")
                    && self.loaded.get()
                    && !self.expired.get()
                    && app.probe_is_locked()
                    && pending.deadline.is_some_and(|d| d > monotonic_now()) =>
            {
                self.deadline.set(pending.deadline.unwrap_or_default());
                app.probe_unlock();
                self.last_generation.borrow_mut().take();
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                Ok(json!({"documentId":app.session.document_id,"generation":self.generation.get().to_string()}))
            }
            "edit.renew"
                if option_env!("CURIE_CAD_TEST_WRITABLE") == Some("1") && self.editing() && pending.deadline.is_some_and(|d| d > monotonic_now()) =>
            {
                self.deadline.set(pending.deadline.unwrap_or_default());
                Ok(json!({}))
            }
            "edit.stop" => {
                self.deadline.set(0.0);
                self.expired.set(true);
                app.probe_force_lock();
                Ok(json!({}))
            }
            "edit.start" | "edit.renew" => Err("edit_unavailable"),
            "snapshot" if self.loaded.get() && (app.session.probe_locked || self.editing()) => {
                let was_locked = app.probe_is_locked();
                let resume = self.editing() && !was_locked;
                // A forced lease-loss lock may hide in-flight work. Recheck all
                // quiescence conditions even when the lock is already held.
                if was_locked {
                    app.probe_unlock();
                }
                if app.probe_lock(ctx).is_err() {
                    if was_locked {
                        app.probe_force_lock();
                    }
                    return self.send_error(&pending, "editor_busy");
                }
                let design = app.session.design();
                if !design.links.is_empty() {
                    if resume && self.editing() {
                        app.probe_unlock();
                    }
                    Err("external_links_unsupported")
                } else {
                    match solidcraft_engine::io::write_design_container(&design) {
                        Ok(bytes) if bytes.len() <= MAX_DOCUMENT_BYTES => {
                            let response = response_envelope(
                                &pending.request,
                                json!({"ok":true,"result":{"documentId":app.session.document_id,"generation":self.generation.get().to_string()}}),
                            );
                            let array = js_sys::Uint8Array::from(bytes.as_slice());
                            let sent = (|| -> Result<(), JsValue> {
                                let value = js_json(&response)?;
                                let payload = js_sys::Reflect::get(&value, &"payload".into())?;
                                let result = js_sys::Reflect::get(&payload, &"result".into())?;
                                let buffer = array.buffer();
                                if !js_sys::Reflect::set(&result, &"bytes".into(), &buffer)? {
                                    return Err(JsValue::from_str("snapshot response unavailable"));
                                }
                                let transfers = js_sys::Array::new();
                                transfers.push(&buffer);
                                pending.port.post_message_with_transferable(&value, &transfers)
                            })();
                            if resume && self.editing() {
                                app.probe_unlock();
                            }
                            if sent.is_ok() {
                                return;
                            }
                            Err("snapshot_transfer_failed")
                        }
                        _ => {
                            if resume && self.editing() {
                                app.probe_unlock();
                            }
                            Err("snapshot_failed")
                        }
                    }
                }
            }
            "snapshot" => Err("snapshot_unavailable"),
            _ => Err("unsupported_capability"),
        };
        let payload = match result {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(code) => json!({"ok":false,"error":{"code":code}}),
        };
        let sent = js_json(&response_envelope(&pending.request, payload)).and_then(|value| pending.port.post_message(&value));
        if sent.is_err() {
            pending.port.close();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.alive.set(false);
        if let Some(window) = web_sys::window()
            && let Some(listener) = self.listener.borrow_mut().take()
        {
            let _ = window.remove_event_listener_with_callback("message", listener.as_ref().unchecked_ref());
        }
        if let Some(port) = self.port.borrow_mut().take() {
            port.set_onmessage(None);
            port.close();
        }
        self.port_listener.borrow_mut().take();
    }
}

fn json_value(value: &JsValue) -> Result<Value, JsValue> {
    let text = js_sys::JSON::stringify(value)?.as_string().ok_or_else(|| JsValue::from_str("not JSON"))?;
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(JsValue::from_str("message too large"));
    }
    serde_json::from_str(&text).map_err(|_| JsValue::from_str("invalid JSON"))
}

fn js_json(value: &Value) -> Result<JsValue, JsValue> {
    js_sys::JSON::parse(&value.to_string())
}

fn close_ports(ports: &js_sys::Array) {
    for value in ports.iter() {
        if let Ok(port) = value.dyn_into::<MessagePort>() {
            port.close();
        }
    }
}

async fn install(
    listener: Rc<RefCell<Option<Listener>>>,
    port_listener: Rc<RefCell<Option<Listener>>>,
    accepted_port: Rc<RefCell<Option<MessagePort>>>,
    alive: Rc<Cell<bool>>,
    pending: Rc<RefCell<Option<Pending>>>,
    deadline: Rc<Cell<f64>>,
    binding: Rc<RefCell<Option<(String, String)>>>,
    ctx: egui::Context,
) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let parent = window.parent()?.ok_or_else(|| JsValue::from_str("no parent"))?;
    if js_sys::Object::is(window.as_ref(), parent.as_ref()) {
        return Err(JsValue::from_str("managed editor must be framed"));
    }
    let options = web_sys::RequestInit::new();
    options.set_cache(web_sys::RequestCache::NoStore);
    options.set_credentials(web_sys::RequestCredentials::Omit);
    options.set_mode(web_sys::RequestMode::SameOrigin);
    options.set_redirect(web_sys::RequestRedirect::Error);
    let response = JsFuture::from(window.fetch_with_str_and_init("/runtime-config.json", &options)).await?.dyn_into::<web_sys::Response>()?;
    let expected_config_url = format!("{}/runtime-config.json", window.location().origin()?);
    if !response.ok() || response.url() != expected_config_url {
        return Err(JsValue::from_str("runtime config unavailable or redirected"));
    }
    let body = response.body().ok_or_else(|| JsValue::from_str("missing runtime config body"))?;
    let reader = body.get_reader().dyn_into::<web_sys::ReadableStreamDefaultReader>()?;
    let mut config = Vec::new();
    loop {
        let result = JsFuture::from(reader.read()).await?;
        if js_sys::Reflect::get(&result, &"done".into())?.as_bool() == Some(true) {
            break;
        }
        let chunk = js_sys::Reflect::get(&result, &"value".into())?.dyn_into::<js_sys::Uint8Array>()?;
        if chunk.length() as usize > 16 * 1024 - config.len() {
            let _ = reader.cancel();
            return Err(JsValue::from_str("runtime config too large"));
        }
        config.extend_from_slice(&chunk.to_vec());
    }
    let text = String::from_utf8(config).map_err(|_| JsValue::from_str("runtime config is not UTF-8"))?;
    let policy = Rc::new(RefCell::new(Policy::from_config(&text).map_err(JsValue::from_str)?));
    if !alive.get() {
        return Err(JsValue::from_str("bridge closed"));
    }
    let allowed = policy.borrow().origins().to_vec();
    let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let is_parent = event.source().is_some_and(|source| js_sys::Object::is(source.as_ref(), parent.as_ref()));
        let ports = event.ports();
        // Check sender before serializing its data; unrelated origins are never parsed.
        if !is_parent || !policy.borrow().origins().iter().any(|origin| origin == &event.origin()) {
            close_ports(&ports);
            return;
        }
        let init_data = event.data();
        if !init_data.is_object() || init_data.is_null() || js_sys::Object::keys(&js_sys::Object::from(init_data.clone())).length() != 5 {
            close_ports(&ports);
            return;
        }
        for key in ["type", "nonce", "designId", "sessionEpoch"] {
            let value = js_sys::Reflect::get(&init_data, &key.into()).ok().and_then(|v| v.as_string());
            if !value.is_some_and(|v| v.len() <= 128) {
                close_ports(&ports);
                return;
            }
        }
        let Ok(version) = js_sys::Reflect::get(&init_data, &"version".into()) else {
            close_ports(&ports);
            return;
        };
        if !version.is_object()
            || version.is_null()
            || js_sys::Object::keys(&js_sys::Object::from(version.clone())).length() != 2
            || js_sys::Reflect::get(&version, &"major".into()).ok().and_then(|v| v.as_f64()) != Some(MAJOR as f64)
            || js_sys::Reflect::get(&version, &"minor".into()).ok().and_then(|v| v.as_f64()) != Some(MINOR as f64)
        {
            close_ports(&ports);
            return;
        }
        let Ok(value) = json_value(&init_data) else {
            close_ports(&ports);
            return;
        };
        let Ok(init) = policy.borrow_mut().accept(&event.origin(), true, ports.length() as usize, &value) else {
            close_ports(&ports);
            return;
        };
        let Ok(port) = ports.get(0).dyn_into::<MessagePort>() else { return };
        let ready = json!({"type":"curie.cad.ready", "version":{"major":MAJOR,"minor":MINOR},
            "nonce":init.nonce, "designId":init.design_id, "sessionEpoch":init.session_epoch});
        let Ok(value) = js_json(&ready) else {
            port.close();
            return;
        };
        if port.post_message(&value).is_err() {
            port.close();
            return;
        }
        // Scope for this task is channel establishment only. Never run document operations here.
        let design_id = init.design_id;
        let epoch = init.session_epoch;
        *binding.borrow_mut() = Some((design_id.clone(), epoch.clone()));
        let reply_port = port.clone();
        let pending = pending.clone();
        let deadline = deadline.clone();
        let ctx = ctx.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
            let ports = message.ports();
            if ports.length() != 0 {
                close_ports(&ports);
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            }
            let data = message.data();
            let Ok(payload) = js_sys::Reflect::get(&data, &"payload".into()) else {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            };
            let Ok(action) = js_sys::Reflect::get(&payload, &"action".into()) else {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            };
            let Some(action) = action.as_string().filter(|s| s.len() <= 32) else {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            };
            // Reject unexpected keys and oversized or non-string scalar values before JSON conversion.
            let top = js_sys::Object::from(data.clone());
            let top_keys = js_sys::Object::keys(&top);
            if top_keys.length() > 8 {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            }
            for key in top_keys.iter() {
                let Some(name) = key.as_string() else {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                };
                if !["version", "kind", "requestId", "sessionEpoch", "designId", "operationId", "expectedGeneration", "payload"]
                    .contains(&name.as_str())
                {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                }
                if name != "payload" && name != "version" {
                    let Ok(value) = js_sys::Reflect::get(&data, &key) else {
                        deadline.set(1.0);
                        reply_port.close();
                        ctx.request_repaint();
                        return;
                    };
                    if !value.as_string().is_some_and(|s| s.len() <= 128) {
                        deadline.set(1.0);
                        reply_port.close();
                        ctx.request_repaint();
                        return;
                    }
                }
            }
            let Ok(version) = js_sys::Reflect::get(&data, &"version".into()) else {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            };
            let version_object = js_sys::Object::from(version.clone());
            if js_sys::Object::keys(&version_object).length() != 2
                || js_sys::Reflect::get(&version, &"major".into()).ok().and_then(|v| v.as_f64()) != Some(MAJOR as f64)
                || js_sys::Reflect::get(&version, &"minor".into()).ok().and_then(|v| v.as_f64()) != Some(MINOR as f64)
            {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            }
            let keys = js_sys::Object::keys(&js_sys::Object::from(payload.clone()));
            // Replace the binary payload before JSON serialization; never stringify its contents.
            let sanitized = js_sys::Object::assign(&js_sys::Object::new(), &js_sys::Object::from(data));
            let clean_payload = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&clean_payload, &"action".into(), &action.clone().into());
            let _ = js_sys::Reflect::set(&sanitized, &"payload".into(), &clean_payload);
            let Ok(request) = json_value(&sanitized.into()) else {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            };
            if !valid_request(&request, &design_id, &epoch) {
                deadline.set(1.0);
                reply_port.close();
                ctx.request_repaint();
                return;
            }
            if action == "edit.stop" {
                deadline.set(1.0);
                ctx.request_repaint();
            }
            if pending.borrow().is_some() {
                if let Ok(value) = js_json(&response_envelope(&request, json!({"ok":false,"error":{"code":"busy"}}))) {
                    let _ = reply_port.post_message(&value);
                }
                return;
            }
            if request.get("expectedGeneration").is_some() {
                if let Ok(value) = js_json(&response_envelope(&request, json!({"ok":false,"error":{"code":"unsupported_precondition"}}))) {
                    let _ = reply_port.post_message(&value);
                }
                return;
            }
            let grant_deadline = if action == "edit.start" || action == "edit.renew" {
                if keys.length() != 2 {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                }
                let ttl = js_sys::Reflect::get(&payload, &"validForMs".into()).ok().and_then(|v| v.as_f64());
                let Some(ttl) = ttl.filter(|v| v.is_finite() && *v >= 1.0 && *v <= 5000.0 && v.fract() == 0.0) else {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                };
                Some(monotonic_now() + ttl)
            } else {
                None
            };
            let bytes = if action == "load" {
                if keys.length() != 2 {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                }
                let Ok(raw) = js_sys::Reflect::get(&payload, &"bytes".into()) else {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                };
                let Ok(buffer) = raw.dyn_into::<js_sys::ArrayBuffer>() else {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                };
                if buffer.byte_length() as usize > MAX_DOCUMENT_BYTES {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                }
                Some(js_sys::Uint8Array::new(&buffer).to_vec())
            } else {
                if grant_deadline.is_none() && keys.length() != 1 {
                    deadline.set(1.0);
                    reply_port.close();
                    ctx.request_repaint();
                    return;
                }
                None
            };
            *pending.borrow_mut() = Some(Pending { request, bytes, port: reply_port.clone(), deadline: grant_deadline });
            ctx.request_repaint();
        });
        port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        port.start();
        *port_listener.borrow_mut() = Some(on_message);
        *accepted_port.borrow_mut() = Some(port);
    });
    window.add_event_listener_with_callback("message", handler.as_ref().unchecked_ref())?;
    *listener.borrow_mut() = Some(handler);
    let listening = js_json(&json!({"type":"curie.cad.listening","version":{"major":MAJOR,"minor":MINOR}}))?;
    let parent = window.parent()?.ok_or_else(|| JsValue::from_str("no parent"))?;
    for origin in allowed {
        parent.post_message(&listening, &origin)?;
    }
    Ok(())
}

fn response_envelope(request: &Value, payload: Value) -> Value {
    let mut out = json!({"version":{"major":MAJOR,"minor":MINOR},"kind":"response","requestId":request["requestId"],"sessionEpoch":request["sessionEpoch"],"designId":request["designId"],"payload":payload});
    if let Some(id) = request.get("operationId") {
        out["operationId"] = id.clone();
    }
    out
}

fn monotonic_now() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(f64::INFINITY)
}
