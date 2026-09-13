use reolink_core::{DeviceInfoSummary, ReolinkClient, VideoFrame};
use tokio_stream::StreamExt;

pub enum AppEvent {
    LoggedIn(DeviceInfoSummary),
    Frame(VideoFrame),
    Failed(String),
}

/// Spawns a dedicated tokio runtime on a background OS thread and drives the
/// whole connect→login→start_video flow there, forwarding progress to the
/// GTK main loop over an `async-channel` (GTK4/GLib are not thread-safe, so
/// no widget is ever touched off the main thread — the receiving end is
/// driven by `glib::spawn_future_local` on the GLib main context).
pub fn spawn_connection(
    uid: String,
    username: String,
    password: String,
    channel_id: u8,
) -> async_channel::Receiver<AppEvent> {
    let (tx, rx) = async_channel::unbounded();

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        runtime.block_on(async move {
            let mut client = match ReolinkClient::connect_by_uid(&uid).await {
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

            let mut frames = match client.start_video(channel_id).await {
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
