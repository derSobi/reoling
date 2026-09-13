# <img src="apps/reoling-linux/data/icons/hicolor/512x512/apps/de.dersobi.reoling.png" width="64" valign="middle"> **Reoling**  

An unofficial client for Reolink® devices — cameras, NVRs, and Home Hubs —
that connects over Reolink's proprietary "Baichuan" P2P protocol using only
a device's UID (no manual IP/port configuration, no cloud account required).

Reoling is not affiliated with, endorsed by, or sponsored by Reolink.
"Reolink" is a trademark of its respective owner.

## Status

Early development. The Baichuan protocol (binary framing, XML payloads,
BCEncrypt/AES encryption, P2P discovery and NAT traversal) and the login
flow are implemented and confirmed working end-to-end against real
hardware. Live video decoding is in progress. There is no packaged release
yet — see [Installation](#installation) below.

## How it works

Reoling implements the Baichuan protocol from scratch, in Rust, based on
observed wire behavior (see [Acknowledgments](#acknowledgments)). It never
requires the official Reolink cloud account or app — you connect directly
to a device by its UID, and Reoling resolves the P2P path itself (direct
connection when reachable, falling back to Reolink's relay
infrastructure).

## Project layout

This is a Cargo workspace:

```text
crates/
├── reolink-protocol/   Wire protocol only: binary framing, XML, BCEncrypt/AES.
│                       No I/O, no async runtime — pure, testable codec.
└── reolink-core/       P2P/UDP transport (UID resolution, NAT traversal,
                        relay fallback) and the client/session API
                        (login, live video) built on reolink-protocol.
apps/
└── reoling-linux/      GTK4 + GStreamer desktop app for Linux.
```

`reolink-core` has no GUI-toolkit dependency, so a future native Windows
app can reuse it directly.

## Installation

Not yet packaged. Planned distribution once the app reaches a usable
state:

- **Ubuntu / Debian**: a Launchpad PPA (`ppa:dersobi/reoling`).
- **Linux, distro-independent**: a `Reoling.AppImage` build.

Until then, build from source (see below).

## Building from source

Requires:

- Rust (edition 2021, `rust-version` 1.75+ — see the workspace
  `Cargo.toml`)
- GTK4 (`>= 4.6`, Ubuntu 22.04's baseline) and its development headers
- GStreamer, including `gstreamer-app`, and a plugin capable of decoding
  H.264 (provided by your distribution)

```bash
cargo build --release -p reoling-linux
cargo run -p reoling-linux
```

To run the full test suite (protocol codec, transport, and session logic —
no real hardware required):

```bash
cargo test --workspace
```

## License

AGPL-3.0-or-later — see [LICENSE](LICENSE) for the full text and
third-party component notices (GTK4, GStreamer, and the Rust crates
Reoling depends on).

## Acknowledgments

Reoling's understanding of the Baichuan protocol was built by reading
(never copying code from) other reverse-engineering efforts, most notably
[neolink](https://github.com/QuantumEntangledAndy/neolink) and Reolink's
own [reolink-cli](https://github.com/reolink/reolink-cli) (LAN-only,
closed-source, documentation only). See [LICENSE](LICENSE) for the full
list and their own licenses.
