//! Manual diagnostic tool, not part of the app: connects to a real device
//! by UID, logs in, and pulls a few video frames. Prompts for username and
//! password interactively (never pass them as CLI args — they'd land in
//! shell history) so run this directly in your own terminal.
//! Usage: cargo run -p reolink-protocol --example probe_login_video -- <UID>
use reolink_core::client::ReolinkClient;
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
    let uid = args
        .get(1)
        .expect("usage: probe_login_video <UID> [--empty-nonce] [--prefer-direct] [--channel N]")
        .clone();
    let empty_nonce_probe = args.iter().any(|a| a == "--empty-nonce");
    let prefer_direct = args.iter().any(|a| a == "--prefer-direct");
    let channel_id: u8 = args
        .iter()
        .position(|a| a == "--channel")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse().expect("--channel expects a number"))
        .unwrap_or(0);
    let username = prompt("username");
    let password = prompt("password");

    println!("Connecting to UID {uid} over P2P{}...", if prefer_direct { " (preferring direct)" } else { "" });
    let start = std::time::Instant::now();
    let connect_result = if prefer_direct {
        ReolinkClient::connect_by_uid_prefer_direct(&uid).await
    } else {
        ReolinkClient::connect_by_uid(&uid).await
    };
    let mut client = match connect_result {
        Ok(c) => {
            println!("P2P connect: SUCCESS in {:?}", start.elapsed());
            c
        }
        Err(e) => {
            println!("P2P connect FAILED after {:?}: {e}", start.elapsed());
            return;
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
    let mut frames = match client.start_video(channel_id).await {
        Ok(f) => {
            println!("start_video: SUCCESS in {:?}", start.elapsed());
            f
        }
        Err(e) => {
            println!("start_video FAILED after {:?}: {e}", start.elapsed());
            return;
        }
    };

    let mut count = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            next = frames.next() => {
                match next {
                    Some(Ok(frame)) => {
                        count += 1;
                        println!("frame #{count}: {} bytes, t={}us", frame.data.len(), frame.microseconds);
                        if count >= 10 {
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
