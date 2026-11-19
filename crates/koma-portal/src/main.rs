//! `xdg-desktop-portal-koma` D-Bus portal backend (`ScreenCast` + `RemoteDesktop` + PipeWire).

mod pipewire_stream;
mod portal_dbus;
mod wayland_bridge;

use std::future::pending;

use anyhow::Context as _;
use pipewire_stream::PipeWireManager;
use portal_dbus::{PortalState, RemoteDesktopPortal, ScreenCastPortal};
use wayland_bridge::WaylandBridge;

const PORTAL_BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.koma";
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let wayland = WaylandBridge::connect().context("failed to initialize Wayland bridge")?;
    let pipewire =
        PipeWireManager::start(wayland.clone()).context("failed to initialize PipeWire manager")?;
    let state = PortalState::new(wayland, pipewire);

    let _conn = zbus::connection::Builder::session()
        .name(PORTAL_BUS_NAME)
        .serve_at(
            PORTAL_OBJECT_PATH,
            ScreenCastPortal {
                state: state.clone(),
            },
        )
        .serve_at(PORTAL_OBJECT_PATH, RemoteDesktopPortal { state })
        .build()
        .await
        .context("failed to register org.freedesktop.impl.portal.desktop.koma on session D-Bus")?;

    tracing::info!(
        bus_name = PORTAL_BUS_NAME,
        object_path = PORTAL_OBJECT_PATH,
        "xdg-desktop-portal-koma ready"
    );

    pending::<()>().await;
    Ok(())
}
