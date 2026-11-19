//! `org.freedesktop.impl.portal.{ScreenCast,RemoteDesktop,Session,Request}` D-Bus backend.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use zbus::Connection;
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

use crate::pipewire_stream::PipeWireManager;
use crate::wayland_bridge::{WaylandBridge, WaylandCommand};

#[derive(Debug, Clone)]
pub struct PortalSessionData {
    pub has_screencast: bool,
    pub has_remotedesktop: bool,
    pub device_types: u32,
    pub cursor_mode: u32,
    pub stream_nodes: Vec<(u32, String)>,
}

impl Default for PortalSessionData {
    fn default() -> Self {
        Self {
            has_screencast: false,
            has_remotedesktop: false,
            device_types: 0,
            cursor_mode: 2, // Embedded cursor by default
            stream_nodes: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct PortalState {
    pub wayland: WaylandBridge,
    pub pipewire: PipeWireManager,
    pub sessions: Arc<Mutex<HashMap<String, PortalSessionData>>>,
}

impl PortalState {
    pub fn new(wayland: WaylandBridge, pipewire: PipeWireManager) -> Self {
        Self {
            wayland,
            pipewire,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn create_session(
        &self,
        conn: &Connection,
        session_handle: &ObjectPath<'_>,
        is_screencast: bool,
        is_remotedesktop: bool,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let key = session_handle.as_str().to_owned();
        {
            let mut map = self.sessions.lock().await;
            let entry = map.entry(key.clone()).or_default();
            entry.has_screencast |= is_screencast;
            entry.has_remotedesktop |= is_remotedesktop;
        }
        let session_iface = PortalSession {
            session_path: OwnedObjectPath::from(session_handle.to_owned()),
            state: self.clone(),
        };
        let _ = conn.object_server().at(session_handle, session_iface).await;
        Ok((0, HashMap::new()))
    }

    async fn start_session(
        &self,
        session_handle: &ObjectPath<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let key = session_handle.as_str().to_owned();
        let (has_sc, has_rd, dev_types, cursor_mode, existing_nodes) = {
            let map = self.sessions.lock().await;
            let Some(sess) = map.get(&key) else {
                return Ok((2, HashMap::new()));
            };
            (
                sess.has_screencast,
                sess.has_remotedesktop,
                sess.device_types,
                sess.cursor_mode,
                sess.stream_nodes.clone(),
            )
        };

        let mut results: HashMap<String, OwnedVariant> = HashMap::new();
        if has_rd {
            let devices = if dev_types == 0 { 3 } else { dev_types };
            if let Ok(v) = OwnedVariant::try_from(Variant::from(devices)) {
                results.insert("devices".to_owned(), v);
            }
        }

        if has_sc {
            let mut stream_entries: Vec<(u32, HashMap<String, Variant<'static>>)> = Vec::new();
            let outputs = self.wayland.list_outputs().unwrap_or_default();
            if existing_nodes.is_empty() {
                let overlay_cursor = cursor_mode != 1;
                let mut new_nodes = Vec::new();

                if let Some(out) = outputs.first() {
                    match self.pipewire.start_stream(
                        out.name.clone(),
                        out.pixel_w,
                        out.pixel_h,
                        overlay_cursor,
                    ) {
                        Ok(node_id) => {
                            new_nodes.push((node_id, out.name.clone()));
                            let mut props: HashMap<String, Variant<'static>> = HashMap::new();
                            props.insert(
                                "position".to_owned(),
                                Variant::from((out.logical_x, out.logical_y)),
                            );
                            props.insert(
                                "size".to_owned(),
                                Variant::from((out.logical_w, out.logical_h)),
                            );
                            props.insert("source_type".to_owned(), Variant::from(1_u32));
                            stream_entries.push((node_id, props));
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "failed to start screencast PipeWire stream");
                            return Ok((2, HashMap::new()));
                        }
                    }
                } else {
                    tracing::warn!("no Wayland outputs available for ScreenCast.Start");
                    return Ok((2, HashMap::new()));
                }

                let mut map = self.sessions.lock().await;
                if let Some(sess) = map.get_mut(&key) {
                    if sess.stream_nodes.is_empty() {
                        sess.stream_nodes = new_nodes;
                    } else {
                        for (dup_id, _) in new_nodes {
                            self.pipewire.stop_stream(dup_id);
                        }
                    }
                } else {
                    for (orphan_id, _) in new_nodes {
                        self.pipewire.stop_stream(orphan_id);
                    }
                    return Ok((2, HashMap::new()));
                }
            } else {
                for (node_id, out_name) in existing_nodes {
                    let mut props: HashMap<String, Variant<'static>> = HashMap::new();
                    if let Some(out) = outputs.iter().find(|o| o.name == out_name) {
                        props.insert(
                            "position".to_owned(),
                            Variant::from((out.logical_x, out.logical_y)),
                        );
                        props.insert(
                            "size".to_owned(),
                            Variant::from((out.logical_w, out.logical_h)),
                        );
                    }
                    props.insert("source_type".to_owned(), Variant::from(1_u32));
                    stream_entries.push((node_id, props));
                }
            }
            if let Ok(v) = OwnedVariant::try_from(Variant::from(stream_entries)) {
                results.insert("streams".to_owned(), v);
            }
        }

        Ok((0, results))
    }

    async fn close_session(&self, session_path: &str) {
        let removed = self.sessions.lock().await.remove(session_path);
        if let Some(sess) = removed {
            for (node_id, _) in sess.stream_nodes {
                self.pipewire.stop_stream(node_id);
            }
        }
    }
}

pub struct ScreenCastPortal {
    pub state: PortalState,
}

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastPortal {
    #[zbus(property)]
    async fn available_source_types(&self) -> u32 {
        1 // Monitor
    }

    #[zbus(property)]
    async fn available_cursor_modes(&self) -> u32 {
        1 | 2 // Hidden | Embedded
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        5
    }

    async fn create_session(
        &self,
        #[zbus(connection)] conn: &Connection,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        self.state
            .create_session(conn, &session_handle, true, false)
            .await
    }

    async fn select_sources(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let cursor_mode = options
            .get("cursor_mode")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(2);
        let mut map = self.state.sessions.lock().await;
        let sess = map.entry(session_handle.as_str().to_owned()).or_default();
        sess.has_screencast = true;
        sess.cursor_mode = cursor_mode;
        Ok((0, HashMap::new()))
    }

    async fn start(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        self.state.start_session(&session_handle).await
    }
}

pub struct RemoteDesktopPortal {
    pub state: PortalState,
}

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktopPortal {
    #[zbus(property)]
    async fn available_device_types(&self) -> u32 {
        1 | 2 | 4 // Keyboard | Pointer | Touchscreen
    }

    #[zbus(property)]
    async fn version(&self) -> u32 {
        2
    }

    async fn create_session(
        &self,
        #[zbus(connection)] conn: &Connection,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        self.state
            .create_session(conn, &session_handle, false, true)
            .await
    }

    async fn select_devices(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let types = options
            .get("types")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(3);
        let mut map = self.state.sessions.lock().await;
        let sess = map.entry(session_handle.as_str().to_owned()).or_default();
        sess.has_remotedesktop = true;
        sess.device_types = types;
        Ok((0, HashMap::new()))
    }

    async fn start(
        &self,
        _handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        self.state.start_session(&session_handle).await
    }

    async fn notify_pointer_motion(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        self.state
            .wayland
            .send(WaylandCommand::PointerMotion { dx, dy });
        Ok(())
    }

    async fn notify_pointer_motion_absolute(
        &self,
        session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        stream: u32,
        x: f64,
        y: f64,
    ) -> zbus::fdo::Result<()> {
        let output_name = {
            let map = self.state.sessions.lock().await;
            map.get(session_handle.as_str()).and_then(|s| {
                s.stream_nodes
                    .iter()
                    .find(|(nid, _)| *nid == stream)
                    .map(|(_, name)| name.clone())
            })
        };
        self.state
            .wayland
            .send(WaylandCommand::PointerMotionAbsolute { output_name, x, y });
        Ok(())
    }

    async fn notify_pointer_button(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        button: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        self.state.wayland.send(WaylandCommand::PointerButton {
            button: button.max(0) as u32,
            pressed: state != 0,
        });
        Ok(())
    }

    async fn notify_pointer_axis(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        dx: f64,
        dy: f64,
    ) -> zbus::fdo::Result<()> {
        self.state
            .wayland
            .send(WaylandCommand::PointerAxis { dx, dy });
        Ok(())
    }

    async fn notify_pointer_axis_discrete(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        axis: u32,
        steps: i32,
    ) -> zbus::fdo::Result<()> {
        self.state
            .wayland
            .send(WaylandCommand::PointerAxisDiscrete { axis, steps });
        Ok(())
    }

    async fn notify_keyboard_keycode(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        keycode: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        self.state.wayland.send(WaylandCommand::KeyboardKeycode {
            keycode: keycode.max(0) as u32,
            pressed: state != 0,
        });
        Ok(())
    }

    async fn notify_keyboard_keysym(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedVariant>,
        keysym: i32,
        state: u32,
    ) -> zbus::fdo::Result<()> {
        self.state.wayland.send(WaylandCommand::KeyboardKeysym {
            keysym: keysym.max(0) as u32,
            pressed: state != 0,
        });
        Ok(())
    }
}

pub struct PortalSession {
    pub session_path: OwnedObjectPath,
    pub state: PortalState,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl PortalSession {
    #[zbus(property)]
    async fn version(&self) -> u32 {
        1
    }

    async fn close(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.state.close_session(self.session_path.as_str()).await;
        let _ = Self::closed(&emitter, HashMap::new()).await;
        Ok(())
    }

    #[zbus(signal)]
    async fn closed(
        emitter: &SignalEmitter<'_>,
        details: HashMap<String, OwnedVariant>,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland_bridge::OutputInfo;

    #[test]
    fn portal_session_defaults_enable_embedded_cursor() {
        let sess = PortalSessionData::default();
        assert_eq!(sess.cursor_mode, 2);
        assert!(!sess.has_screencast);
        assert!(!sess.has_remotedesktop);
    }

    #[tokio::test]
    async fn combined_session_tracks_devices_cursor_mode_and_close() {
        let (wayland, rx) = WaylandBridge::for_test();
        drop(rx);
        let pipewire = PipeWireManager::for_test();
        let state = PortalState::new(wayland, pipewire);
        let session_path = "/org/freedesktop/portal/desktop/session/test/s1";

        {
            let mut map = state.sessions.lock().await;
            let entry = map.entry(session_path.to_owned()).or_default();
            entry.has_remotedesktop = true;
            entry.has_screencast = true;
            entry.device_types = 3;
            entry.cursor_mode = 4;
            entry.stream_nodes.push((42, "DP-1".to_owned()));
        }

        let (code, results) = state
            .start_session(&ObjectPath::try_from(session_path).expect("valid path"))
            .await
            .expect("start_session");
        assert_eq!(code, 0);
        assert!(results.contains_key("devices"));
        let streams = results.get("streams").expect("streams result");
        assert_eq!(streams.value_signature(), "a(ua{sv})");

        state.close_session(session_path).await;
        assert!(!state.sessions.lock().await.contains_key(session_path));
    }

    #[tokio::test]
    async fn start_session_for_unknown_handle_returns_cancelled() {
        let (wayland, _rx) = WaylandBridge::for_test();
        let pipewire = PipeWireManager::for_test();
        let state = PortalState::new(wayland, pipewire);
        let (code, results) = state
            .start_session(
                &ObjectPath::try_from("/org/freedesktop/portal/desktop/session/test/missing")
                    .expect("valid path"),
            )
            .await
            .expect("start_session");
        assert_eq!(code, 2);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn remote_desktop_input_forwards_commands_to_wayland_bridge() {
        let (wayland, rx) = WaylandBridge::for_test();
        let pipewire = PipeWireManager::for_test();
        let state = PortalState::new(wayland, pipewire);
        let rd = RemoteDesktopPortal {
            state: state.clone(),
        };
        let session_path = "/org/freedesktop/portal/desktop/session/test/s2";

        {
            let mut map = state.sessions.lock().await;
            let entry = map.entry(session_path.to_owned()).or_default();
            entry.has_remotedesktop = true;
            entry.stream_nodes.push((99, "eDP-1".to_owned()));
        }

        let obj_path = ObjectPath::try_from(session_path).expect("valid path");
        rd.notify_pointer_motion_absolute(obj_path.clone(), HashMap::new(), 99, 120.5, 240.25)
            .await
            .expect("pointer motion absolute");
        rd.notify_pointer_button(obj_path.clone(), HashMap::new(), 272, 1)
            .await
            .expect("pointer button");
        rd.notify_keyboard_keysym(obj_path, HashMap::new(), 0x0061, 1)
            .await
            .expect("keyboard keysym");

        match rx.try_recv().expect("pointer motion command") {
            WaylandCommand::PointerMotionAbsolute { output_name, x, y } => {
                assert_eq!(output_name.as_deref(), Some("eDP-1"));
                assert!((x - 120.5).abs() < f64::EPSILON);
                assert!((y - 240.25).abs() < f64::EPSILON);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match rx.try_recv().expect("pointer button command") {
            WaylandCommand::PointerButton { button, pressed } => {
                assert_eq!(button, 272);
                assert!(pressed);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match rx.try_recv().expect("keyboard keysym command") {
            WaylandCommand::KeyboardKeysym { keysym, pressed } => {
                assert_eq!(keysym, 0x0061);
                assert!(pressed);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // Ensure OutputInfo can be constructed cleanly for unit tests.
        let out = OutputInfo {
            name: "eDP-1".to_owned(),
            description: "Built-in Display".to_owned(),
            logical_x: 0,
            logical_y: 0,
            logical_w: 1920,
            logical_h: 1200,
            pixel_w: 1920,
            pixel_h: 1200,
        };
        assert_eq!(out.name, "eDP-1");
    }
}
