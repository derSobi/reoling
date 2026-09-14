use reolink_core::{DeviceInfoSummary, ReolinkClient, StreamQuality, VideoFrame};
use std::net::IpAddr;
use tokio_stream::StreamExt;

pub enum AppEvent {
    LoggedIn(DeviceInfoSummary),
    Frame(VideoFrame),
    Failed(String),
}

/// How the user chose to reach the device — set by the explicit UID/IP
/// toggle in the connect dialog, never inferred or auto-detected.
pub enum ConnectTarget {
    Uid(String),
    Ip { addr: IpAddr, port: u16 },
}

/// Spawns a dedicated tokio runtime on a background OS thread and drives the
/// whole connect→login→start_video flow there, forwarding progress to the
/// GTK main loop over an `async-channel` (GTK4/GLib are not thread-safe, so
/// no widget is ever touched off the main thread — the receiving end is
/// driven by `glib::spawn_future_local` on the GLib main context).
pub fn spawn_connection(
    target: ConnectTarget,
    username: String,
    password: String,
    channel_id: u8,
    quality: StreamQuality,
) -> async_channel::Receiver<AppEvent> {
    let (tx, rx) = async_channel::unbounded();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(async move {
            let connect_result = match target {
                // Prefers TCP:9000 over the plain UDP/P2P session whenever
                // UID resolution reaches the device directly (not via
                // relay) — confirmed against real hardware 2026-09-14:
                // the UDP path stutters in ~1s bursts (its own
                // retransmission bitmap, `bcudp::model::UdpAck`, isn't
                // implemented), while TCP's in-kernel retransmission and
                // ordering delivers frames smoothly. See
                // `ReolinkClient::connect_by_uid_prefer_tcp`.
                ConnectTarget::Uid(uid) => ReolinkClient::connect_by_uid_prefer_tcp(&uid).await,
                ConnectTarget::Ip { addr, port } => ReolinkClient::connect_by_ip(addr, port).await,
            };
            let mut client = match connect_result {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                    return;
                }
            };
            let device_info = match client.login(&username, &password).await {
                Ok(info) => info,
                Err(e) => {
                    let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                    return;
                }
            };
            let _ = tx.send(AppEvent::LoggedIn(device_info)).await;

            let mut frames = match client.start_video(channel_id, quality).await {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                    return;
                }
            };
            while let Some(frame) = frames.next().await {
                match frame {
                    Ok(frame) => {
                        if tx.send(AppEvent::Frame(frame)).await.is_err() {
                            break; // UI side dropped the receiver (window closed)
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(AppEvent::Failed(e.to_string())).await;
                        break;
                    }
                }
            }
        });
    });

    rx
}
