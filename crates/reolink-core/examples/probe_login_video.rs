//! Manual diagnostic tool, not part of the app: connects to a real device
//! (by UID/P2P, or directly by IP over TCP with `--tcp`), logs in, and
//! pulls a few video frames. Prompts for username and password
//! interactively (never pass them as CLI args — they'd land in shell
//! history) so run this directly in your own terminal.
//! Usage: cargo run -p reolink-protocol --example probe_login_video -- <UID>
//!    or: cargo run -p reolink-protocol --example probe_login_video -- --tcp <IP:PORT>
use reolink_core::client::{ReolinkClient, StreamQuality};
use std::io::Write;
use tokio_stream::StreamExt;

fn prompt(label: &str) -> String {
    print!("{label}: ");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    line.trim().to_string()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let empty_nonce_probe = args.iter().any(|a| a == "--empty-nonce");
    let prefer_direct = args.iter().any(|a| a == "--prefer-direct");
    let prefer_tcp = args.iter().any(|a| a == "--prefer-tcp");
    let quality = if args.iter().any(|a| a == "--sub-stream") { StreamQuality::Sub } else { StreamQuality::Main };
    let channel_id: u8 = args
        .iter()
        .position(|a| a == "--channel")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse().expect("--channel expects a number"))
        .unwrap_or(0);
    let dump_path = args
        .iter()
        .position(|a| a == "--dump-file")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let tcp_addr = args
        .iter()
        .position(|a| a == "--tcp")
        .and_then(|i| args.get(i + 1))
        .cloned();
    let username = prompt("username");
    let password = prompt("password");

    let start = std::time::Instant::now();
    let mut client = if let Some(addr) = tcp_addr {
        let socket_addr: std::net::SocketAddr =
            addr.parse().expect("--tcp expects IP:PORT, e.g. 172.17.3.30:48474");
        println!("Connecting to {socket_addr} directly over TCP...");
        match ReolinkClient::connect_by_ip(socket_addr.ip(), socket_addr.port()).await {
            Ok(c) => {
                println!("TCP connect: SUCCESS in {:?}", start.elapsed());
                c
            }
            Err(e) => {
                println!("TCP connect FAILED after {:?}: {e}", start.elapsed());
                return;
            }
        }
    } else {
        let uid = args
            .get(1)
            .expect("usage: probe_login_video <UID> [--empty-nonce] [--prefer-direct] [--prefer-tcp] [--channel N] [--dump-file PATH] [--sub-stream]\n   or: probe_login_video --tcp <IP:PORT> [same flags]")
            .clone();
        println!(
            "Connecting to UID {uid} over P2P{}...",
            if prefer_tcp { " (preferring direct, upgrading to TCP:9000 if reachable)" }
            else if prefer_direct { " (preferring direct)" }
            else { "" }
        );
        let connect_result = if prefer_tcp {
            ReolinkClient::connect_by_uid_prefer_tcp(&uid).await
        } else if prefer_direct {
            ReolinkClient::connect_by_uid_prefer_direct(&uid).await
        } else {
            ReolinkClient::connect_by_uid(&uid).await
        };
        match connect_result {
            Ok(c) => {
                println!("P2P connect: SUCCESS in {:?}", start.elapsed());
                c
            }
            Err(e) => {
                println!("P2P connect FAILED after {:?}: {e}", start.elapsed());
                return;
            }
        }
    };

    println!("peer address (direct vs relay): {}", client.peer_addr().await);
    println!("relay login nonce available: {}", client.has_relay_nonce().await);

    println!("channel: {channel_id}");
    let start = std::time::Instant::now();
    let login_result = if empty_nonce_probe {
        println!("(using login_probe_empty_nonce — diagnostic only)");
        client.login_probe_empty_nonce(&username, &password).await
    } else {
        client.login(&username, &password).await
    };
    let device_info = match login_result {
        Ok(info) => {
            println!("login: SUCCESS in {:?}: {info:?}", start.elapsed());
            info
        }
        Err(e) => {
            println!("login FAILED after {:?}: {e}", start.elapsed());
            return;
        }
    };
    let _ = device_info;

    let start = std::time::Instant::now();
    let mut frames = match client.start_video(channel_id, quality).await {
        Ok(f) => {
            println!("start_video: SUCCESS in {:?}", start.elapsed());
            f
        }
        Err(e) => {
            println!("start_video FAILED after {:?}: {e}", start.elapsed());
            return;
        }
    };

    let mut dump_file = dump_path.as_ref().map(|p| {
        std::fs::File::create(p).unwrap_or_else(|e| panic!("could not create dump file {p}: {e}"))
    });
    let frame_target = if dump_file.is_some() { 300 } else { 10 };

    let mut count = 0;
    let mut last_received = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            next = frames.next() => {
                match next {
                    Some(Ok(frame)) => {
                        count += 1;
                        let gap = last_received.elapsed();
                        last_received = std::time::Instant::now();
                        println!("frame #{count}: {} bytes, t={}us, keyframe={}, video_type={:?}, gap_since_last={gap:?}", frame.data.len(), frame.microseconds, frame.is_keyframe, frame.video_type);
                        if let Some(file) = dump_file.as_mut() {
                            file.write_all(&frame.data).expect("failed writing dump file");
                        } else {
                            let dump_len = frame.data.len().min(80);
                            let hex: String = frame.data[..dump_len].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
                            println!("  first {dump_len} bytes: {hex}");
                        }
                        if count >= frame_target {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        println!("frame stream error: {e}");
                        break;
                    }
                    None => {
                        println!("frame stream ended");
                        break;
                    }
                }
            }
        }
    }
    println!("received {count} video frames total");

    let _ = client.stop_video(channel_id).await;
    let _ = client.logout().await;
}
