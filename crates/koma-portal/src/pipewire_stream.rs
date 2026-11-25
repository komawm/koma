//! PipeWire video stream manager for `org.freedesktop.impl.portal.ScreenCast`.

#[cfg(target_os = "linux")]
mod linux_pw {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::Context as _;
    use pipewire::context::ContextRc;
    use pipewire::core::CoreRc;
    use pipewire::main_loop::MainLoopRc;
    use pipewire::properties::PropertiesBox;
    use pipewire::spa::buffer::DataType;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::pod::deserialize::PodDeserializer;
    use pipewire::spa::pod::serialize::PodSerializer;
    use pipewire::spa::pod::{self, ChoiceValue, Pod, Value};
    use pipewire::spa::sys::{
        SPA_ID_INVALID, SPA_PARAM_BUFFERS_blocks, SPA_PARAM_BUFFERS_buffers,
        SPA_PARAM_BUFFERS_dataType, SPA_PARAM_BUFFERS_size, SPA_PARAM_BUFFERS_stride,
    };
    use pipewire::spa::utils::{
        Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
    };
    use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};

    use crate::wayland_bridge::WaylandBridge;

    pub enum PwCommand {
        StartStream {
            output_name: String,
            width: u32,
            height: u32,
            overlay_cursor: bool,
            reply: mpsc::Sender<anyhow::Result<u32>>,
        },
        StopStream {
            node_id: u32,
        },
    }

    #[derive(Clone)]
    pub struct PipeWireManager {
        tx: pipewire::channel::Sender<PwCommand>,
    }

    impl PipeWireManager {
        #[cfg(test)]
        pub fn for_test() -> Self {
            let (tx, _rx) = pipewire::channel::channel();
            Self { tx }
        }

        pub fn start(wayland: WaylandBridge) -> anyhow::Result<Self> {
            let (tx, rx) = pipewire::channel::channel();
            let (ready_tx, ready_rx) = mpsc::channel();

            std::thread::Builder::new()
                .name("koma-portal-pipewire".to_owned())
                .spawn(move || {
                    if let Err(err) = run_pipewire_loop(wayland, rx, ready_tx) {
                        tracing::error!(error = %err, "pipewire loop exited with error");
                    }
                })
                .context("failed to spawn pipewire thread")?;

            ready_rx
                .recv()
                .context("pipewire initialization aborted")??;
            Ok(Self { tx })
        }

        pub fn start_stream(
            &self,
            output_name: String,
            width: u32,
            height: u32,
            overlay_cursor: bool,
        ) -> anyhow::Result<u32> {
            let (reply_tx, reply_rx) = mpsc::channel();
            self.tx
                .send(PwCommand::StartStream {
                    output_name,
                    width,
                    height,
                    overlay_cursor,
                    reply: reply_tx,
                })
                .map_err(|_| anyhow::anyhow!("pipewire channel closed"))?;
            reply_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|err| anyhow::anyhow!("pipewire StartStream timed out or closed: {err}"))?
        }

        pub fn stop_stream(&self, node_id: u32) {
            let _ = self.tx.send(PwCommand::StopStream { node_id });
        }
    }

    struct ActiveStream {
        stream: StreamRc,
        _listener: StreamListener<()>,
        inner: Rc<RefCell<StreamInner>>,
    }

    struct StreamInner {
        output_name: String,
        width: u32,
        height: u32,
        overlay_cursor: bool,
        is_streaming: bool,
        node_id: Option<u32>,
        reply: Option<mpsc::Sender<anyhow::Result<u32>>>,
    }

    fn run_pipewire_loop(
        wayland: WaylandBridge,
        rx: pipewire::channel::Receiver<PwCommand>,
        ready_tx: mpsc::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        pipewire::init();

        let main_loop = match MainLoopRc::new(None) {
            Ok(ml) => ml,
            Err(err) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!("MainLoopRc::new failed: {err}")));
                return Ok(());
            }
        };
        let context = match ContextRc::new(&main_loop, None) {
            Ok(c) => c,
            Err(err) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!("ContextRc::new failed: {err}")));
                return Ok(());
            }
        };
        let core = match context.connect_rc(None) {
            Ok(c) => c,
            Err(err) => {
                let _ = ready_tx.send(Err(anyhow::anyhow!("PipeWire connect failed: {err}")));
                return Ok(());
            }
        };

        let streams: Rc<RefCell<HashMap<u32, ActiveStream>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let pending_streams: Rc<RefCell<Vec<ActiveStream>>> = Rc::new(RefCell::new(Vec::new()));

        let streams_cmd = Rc::clone(&streams);
        let pending_cmd = Rc::clone(&pending_streams);
        let core_cmd = core.clone();
        let _rx_guard = rx.attach(main_loop.loop_(), move |cmd| match cmd {
            PwCommand::StartStream {
                output_name,
                width,
                height,
                overlay_cursor,
                reply,
            } => {
                if let Err(err) = create_stream(
                    &core_cmd,
                    &streams_cmd,
                    &pending_cmd,
                    output_name,
                    width,
                    height,
                    overlay_cursor,
                    reply,
                ) {
                    tracing::error!(error = %err, "failed to start PipeWire stream");
                }
            }
            PwCommand::StopStream { node_id } => {
                streams_cmd.borrow_mut().remove(&node_id);
                pending_cmd
                    .borrow_mut()
                    .retain(|s| s.inner.borrow().node_id != Some(node_id));
            }
        });

        let streams_timer = Rc::clone(&streams);
        let wayland_timer = wayland;
        let timer = main_loop.loop_().add_timer(move |_| {
            let mut map = streams_timer.borrow_mut();
            for active in map.values_mut() {
                let (is_streaming, output_name, overlay_cursor) = {
                    let inner = active.inner.borrow();
                    (
                        inner.is_streaming,
                        inner.output_name.clone(),
                        inner.overlay_cursor,
                    )
                };
                if !is_streaming {
                    continue;
                }
                let Ok(frame) = wayland_timer.capture_frame(Some(output_name), overlay_cursor)
                else {
                    continue;
                };
                let (cur_w, cur_h) = {
                    let g = active.inner.borrow();
                    (g.width, g.height)
                };
                if frame.width != cur_w || frame.height != cur_h {
                    {
                        let mut g = active.inner.borrow_mut();
                        g.width = frame.width.max(1);
                        g.height = frame.height.max(1);
                    }
                    let fmt_bytes = build_format_pod_bytes(frame.width.max(1), frame.height.max(1));
                    if let Some(fmt_pod) = Pod::from_bytes(&fmt_bytes) {
                        let mut params = [fmt_pod];
                        let _ = active.stream.update_params(&mut params);
                    }
                    continue;
                }
                if let Some(mut pw_buf) = active.stream.dequeue_buffer() {
                    let datas = pw_buf.datas_mut();
                    if let Some(data) = datas.first_mut()
                        && let Some(slice) = data.data()
                    {
                        let copy_len = slice.len().min(frame.pixels.len());
                        slice[..copy_len].copy_from_slice(&frame.pixels[..copy_len]);
                        let chunk = data.chunk_mut();
                        *chunk.offset_mut() = 0;
                        *chunk.stride_mut() = frame.stride as i32;
                        *chunk.size_mut() = copy_len as u32;
                    }
                }
            }
        });
        let _ = timer.update_timer(
            Some(Duration::from_millis(33)),
            Some(Duration::from_millis(33)),
        );

        let _ = ready_tx.send(Ok(()));
        main_loop.run();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn create_stream(
        core: &CoreRc,
        streams: &Rc<RefCell<HashMap<u32, ActiveStream>>>,
        pending: &Rc<RefCell<Vec<ActiveStream>>>,
        output_name: String,
        width: u32,
        height: u32,
        overlay_cursor: bool,
        reply: mpsc::Sender<anyhow::Result<u32>>,
    ) -> anyhow::Result<()> {
        let stream = StreamRc::new(core.clone(), "koma-screen-cast", PropertiesBox::new())
            .map_err(|e| anyhow::anyhow!("StreamRc::new failed: {e}"))?;

        let inner = Rc::new(RefCell::new(StreamInner {
            output_name,
            width: width.max(1),
            height: height.max(1),
            overlay_cursor,
            is_streaming: false,
            node_id: None,
            reply: Some(reply),
        }));

        let inner_state = Rc::clone(&inner);
        let streams_state = Rc::clone(streams);
        let pending_state = Rc::clone(pending);

        let inner_param = Rc::clone(&inner);

        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |stream, _, _old, new| {
                let nid = stream.node_id();
                let mut guard = inner_state.borrow_mut();
                guard.is_streaming = new == StreamState::Streaming;
                if matches!(new, StreamState::Error(_)) && guard.node_id.is_none() {
                    if let Some(tx) = guard.reply.take() {
                        let _ =
                            tx.send(Err(anyhow::anyhow!("pipewire stream entered error state")));
                    }
                    drop(guard);
                    let mut p = pending_state.borrow_mut();
                    p.retain(|s| {
                        s.inner.borrow().reply.is_some() || s.inner.borrow().node_id.is_some()
                    });
                    return;
                }
                if nid != SPA_ID_INVALID && guard.node_id.is_none() {
                    guard.node_id = Some(nid);
                    if let Some(tx) = guard.reply.take() {
                        let _ = tx.send(Ok(nid));
                    }
                    drop(guard);
                    let mut p = pending_state.borrow_mut();
                    if let Some(pos) = p.iter().position(|s| s.inner.borrow().node_id == Some(nid))
                    {
                        let act = p.remove(pos);
                        streams_state.borrow_mut().insert(nid, act);
                    }
                }
            })
            .param_changed(move |stream, _, id, param| {
                if id != ParamType::Format.as_raw() || param.is_none() {
                    return;
                }
                let guard = inner_param.borrow();
                let stride = (guard.width * 4) as i32;
                let size = stride * guard.height as i32;
                let buf_bytes = build_buffers_pod_bytes(size, stride);
                if let Some(buf_pod) = Pod::from_bytes(&buf_bytes) {
                    let mut params = [buf_pod];
                    if let Err(err) = stream.update_params(&mut params) {
                        tracing::warn!(error = ?err, "failed to update PipeWire buffer params");
                    }
                }
            })
            .register()
            .map_err(|e| anyhow::anyhow!("stream listener register failed: {e}"))?;

        let fmt_bytes = build_format_pod_bytes(width.max(1), height.max(1));
        let fmt_pod = Pod::from_bytes(&fmt_bytes).context("invalid format pod bytes")?;
        let mut params = [fmt_pod];

        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|e| anyhow::anyhow!("stream connect failed: {e}"))?;

        pending.borrow_mut().push(ActiveStream {
            stream,
            _listener: listener,
            inner,
        });
        Ok(())
    }

    fn build_buffers_pod_bytes(size: i32, stride: i32) -> Vec<u8> {
        let buffers_obj = pod::object!(
            SpaTypes::ObjectParamBuffers,
            ParamType::Buffers,
            pod::Property::new(
                SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 16,
                    },
                ))),
            ),
            pod::Property::new(SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            pod::Property::new(SPA_PARAM_BUFFERS_size, Value::Int(size)),
            pod::Property::new(SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            pod::Property::new(
                SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: 1 << DataType::MemFd.as_raw(),
                        flags: vec![1 << DataType::MemFd.as_raw()],
                    },
                ))),
            ),
        );
        serialize_pod_object(buffers_obj)
    }

    fn build_format_pod_bytes(width: u32, height: u32) -> Vec<u8> {
        let format_obj = pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::EnumFormat,
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(
                FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::BGRx,
                VideoFormat::BGRx,
                VideoFormat::BGRA
            ),
            pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle { width, height }
            ),
            pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                Fraction { num: 30, denom: 1 }
            ),
        );
        serialize_pod_object(format_obj)
    }

    fn serialize_pod_object(obj: pod::Object) -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = PodSerializer::serialize(Cursor::new(&mut buf), &Value::Object(obj));
        // Validate roundtrip when deserializing in debug builds.
        debug_assert!(PodDeserializer::deserialize_any_from(&buf).is_ok());
        buf
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::wayland_bridge::{CapturedFrame, WaylandCommand};
        use pipewire::spa::sys::SPA_DATA_MemPtr;

        #[test]
        fn buffers_pod_uses_memfd_flags_and_range() {
            let stride = 2560 * 4;
            let size = stride * 1600;
            let bytes = build_buffers_pod_bytes(size, stride);
            let (_, val) =
                PodDeserializer::deserialize_any_from(&bytes).expect("valid buffers POD");
            let Value::Object(obj) = val else {
                panic!("expected Object POD, got {val:?}");
            };
            assert_eq!(obj.type_, SpaTypes::ObjectParamBuffers.as_raw());
            assert_eq!(obj.id, ParamType::Buffers.as_raw());

            let prop = |key: u32| -> &Value {
                &obj.properties
                    .iter()
                    .find(|p| p.key == key)
                    .unwrap_or_else(|| panic!("missing property {key}"))
                    .value
            };

            assert_eq!(
                prop(SPA_PARAM_BUFFERS_buffers),
                &Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 16,
                    },
                )))
            );
            assert_eq!(prop(SPA_PARAM_BUFFERS_blocks), &Value::Int(1));
            assert_eq!(prop(SPA_PARAM_BUFFERS_size), &Value::Int(size));
            assert_eq!(prop(SPA_PARAM_BUFFERS_stride), &Value::Int(stride));
            assert_eq!(
                prop(SPA_PARAM_BUFFERS_dataType),
                &Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: 1 << DataType::MemFd.as_raw(),
                        flags: vec![1 << DataType::MemFd.as_raw()],
                    },
                )))
            );
            assert_ne!(
                prop(SPA_PARAM_BUFFERS_dataType),
                &Value::Int(1 << SPA_DATA_MemPtr)
            );
        }

        #[test]
        fn format_pod_serializes_raw_video_bgra_and_bgrx() {
            let bytes = build_format_pod_bytes(1920, 1080);
            let (_, val) = PodDeserializer::deserialize_any_from(&bytes).expect("valid format POD");
            let Value::Object(obj) = val else {
                panic!("expected Object POD, got {val:?}");
            };
            assert_eq!(obj.type_, SpaTypes::ObjectParamFormat.as_raw());
            assert_eq!(obj.id, ParamType::EnumFormat.as_raw());
            assert!(
                obj.properties
                    .iter()
                    .any(|p| p.key == FormatProperties::VideoSize.as_raw()
                        && p.value
                            == Value::Rectangle(Rectangle {
                                width: 1920,
                                height: 1080
                            }))
            );
            assert!(
                obj.properties
                    .iter()
                    .any(|p| p.key == FormatProperties::VideoFramerate.as_raw()
                        && p.value == Value::Fraction(Fraction { num: 30, denom: 1 }))
            );
        }

        #[test]
        fn live_pipewire_consumer_negotiates_memfd_and_receives_frame() {
            let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
                return;
            };
            if !std::path::Path::new(&runtime_dir)
                .join("pipewire-0")
                .exists()
            {
                return;
            }

            let (wayland, rx) = WaylandBridge::for_test();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_wl = std::sync::Arc::clone(&stop);
            let wayland_thread = std::thread::spawn(move || {
                while !stop_wl.load(std::sync::atomic::Ordering::Relaxed) {
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(WaylandCommand::CaptureFrame { reply, .. }) => {
                            let w = 64u32;
                            let h = 64u32;
                            let stride = w * 4;
                            let pixels = vec![0x42u8; (stride * h) as usize];
                            let _ = reply.send(Ok(CapturedFrame {
                                width: w,
                                height: h,
                                stride,
                                format: 0,
                                pixels,
                            }));
                        }
                        Ok(_) => {}
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            });

            let Ok(mgr) = PipeWireManager::start(wayland) else {
                return;
            };
            let node_id = mgr
                .start_stream("TEST-1".to_owned(), 64, 64, false)
                .expect("start_stream");

            let (done_tx, done_rx) = mpsc::channel();
            let consumer_thread = std::thread::spawn(move || {
                pipewire::init();
                let main_loop = MainLoopRc::new(None).expect("consumer main loop");
                let context = ContextRc::new(&main_loop, None).expect("consumer context");
                let core = context.connect_rc(None).expect("consumer core");
                let stream = StreamRc::new(core, "koma-portal-test-consumer", PropertiesBox::new())
                    .expect("consumer stream");

                let ml_quit = main_loop.clone();
                let ml_err = main_loop.clone();
                let tx_frame = done_tx.clone();
                let tx_err = done_tx;
                let _listener = stream
                    .add_local_listener_with_user_data(())
                    .state_changed(move |_, _, _, new| {
                        if let StreamState::Error(msg) = new {
                            let _ = tx_err.send(Err(format!("consumer stream error: {msg}")));
                            ml_err.quit();
                        }
                    })
                    .process(move |stream, _| {
                        if let Some(mut buf) = stream.dequeue_buffer()
                            && let Some(data) = buf.datas_mut().first_mut()
                        {
                            let chunk_size = data.chunk().size();
                            let has_data = data.data().is_some_and(|s| !s.is_empty());
                            if chunk_size > 0 && has_data {
                                let _ = tx_frame.send(Ok(chunk_size));
                                ml_quit.quit();
                            }
                        }
                    })
                    .register()
                    .expect("consumer listener");

                let fmt_bytes = build_format_pod_bytes(64, 64);
                let fmt_pod = Pod::from_bytes(&fmt_bytes).expect("consumer format pod");
                let mut params = [fmt_pod];
                stream
                    .connect(
                        Direction::Input,
                        Some(node_id),
                        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                        &mut params,
                    )
                    .expect("consumer connect");

                let ml_timeout = main_loop.clone();
                let timer = main_loop.loop_().add_timer(move |_| {
                    ml_timeout.quit();
                });
                let _ = timer.update_timer(Some(Duration::from_secs(5)), None);
                main_loop.run();
            });

            let result = done_rx
                .recv_timeout(Duration::from_secs(6))
                .expect("consumer should report frame or error");
            mgr.stop_stream(node_id);
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            drop(mgr);
            let _ = consumer_thread.join();
            let _ = wayland_thread.join();

            let chunk_size = result.expect("PipeWire consumer must negotiate MemFd without error");
            assert_eq!(chunk_size, 64 * 64 * 4);
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux_pw::PipeWireManager;

#[cfg(not(target_os = "linux"))]
#[derive(Clone)]
pub struct PipeWireManager;

#[cfg(not(target_os = "linux"))]
impl PipeWireManager {
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self
    }

    pub fn start(_wayland: crate::wayland_bridge::WaylandBridge) -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn start_stream(
        &self,
        _output_name: String,
        _width: u32,
        _height: u32,
        _overlay_cursor: bool,
    ) -> anyhow::Result<u32> {
        anyhow::bail!("PipeWire screencast streaming is only supported on Linux")
    }

    pub fn stop_stream(&self, _node_id: u32) {}
}
