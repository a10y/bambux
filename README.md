# Bambu X Studio

A native Rust/GPUI studio viewer for monitoring a Bambu Lab printer directly over the local network. It uses the same [Zeron ZUI fork](https://github.com/zeronsh/zui), `gpui-base` foundation, Geist typography, frosted window composition, and semantic surface language as [Zeron](https://github.com/zeronsh/zeron).

![Rust](https://img.shields.io/badge/Rust-2024-8b7cf6) ![UI](https://img.shields.io/badge/UI-GPUI-34d399)

## Features

- Live MQTT-over-TLS telemetry from port `8883`
- Automatic serial discovery through `device/+/report` (only IP and access code are required)
- Bambu partial-report merging and a `pushall` request after discovery
- Interactive orbit/zoom 3D printer view that tracks print height and progress
- Nozzle, bed, chamber, layer, job, ETA, Wi-Fi, and speed data parsing
- In-session MQTT event history and temperature/progress plots
- Connectivity diagnostics for the required LAN services:
  - `8883` — MQTT/TLS live telemetry
  - `990` — implicit FTPS file/job endpoint
  - `3000` — Bambu device service
  - `6000` — Bambu device service
- Persistent printer configuration with owner-only permissions on Unix

## Run

```bash
cargo run --release
```

Open **Configuration**, then enter:

1. The printer's local IP address or hostname.
2. Its **LAN-only access code**, found under the printer's network settings.

> Bambu calls this credential the *LAN-only access code*. It is not a WAN credential. This application connects directly to the printer and does not use Bambu Cloud.

The MQTT username is the printer-defined `bblp`. The app subscribes to `device/+/report`, discovers the serial from the first report topic, and requests a complete state snapshot.

## Security

Bambu printers serve a device-generated, self-signed certificate. Bambu X uses TLS so credentials and telemetry are encrypted, but accepts that self-signed certificate because it cannot be validated through public WebPKI. This does **not** protect against an active attacker on an untrusted network.

- Use the app only on a trusted private LAN.
- Never port-forward `8883`, `990`, `3000`, or `6000` to the internet.
- Configuration is written to the platform config directory as `config.toml`; Unix permissions are set to `0600`.

## Design notes

This project intentionally tracks Zeron's actual UI stack rather than approximating it in another toolkit:

- Zeron's pinned `zeronsh/zui` GPUI fork
- Zeron's pinned `zeronsh/gpui-component` `gpui-base` crate
- Bundled Geist and Geist Mono fonts under the SIL Open Font License
- The exact Zeron Dark background/shell/card/text/accent tokens
- Its 38px custom titlebar, 10px panels, 6–8px controls, hairline borders, translucent window shell, and top-lit glass control recipe
- GPUI canvas rendering for both the interactive digital twin and telemetry plots

The app keeps a focused local theme layer instead of depending on the full `zeron-ui` crate and its unrelated agent, editor, terminal, and synchronization subsystems.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Core modules:

- `src/network.rs` — TLS MQTT worker, report subscription, and service probes
- `src/model.rs` — partial JSON merge, Bambu status model, bounded history
- `src/printer_view.rs` — GPUI-canvas interactive 3D digital twin
- `src/theme.rs` — focused port of Zeron's GPUI semantic theme and glass surfaces
- `src/app.rs` — overview, history, and configuration UI
