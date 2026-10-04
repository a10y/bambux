use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::mpsc, time};

use crate::model::PrinterConfig;

// rumqttc defaults to a 10 KiB inbound packet. Bambu's initial `pushall`
// snapshot commonly exceeds that (especially with AMS data), while normal
// delta reports remain much smaller. Keep a bounded but comfortable ceiling.
const MAX_INCOMING_PACKET_SIZE: usize = 512 * 1024;
const MAX_OUTGOING_PACKET_SIZE: usize = 64 * 1024;

#[derive(Debug)]
pub enum NetworkCommand {
    Connect(PrinterConfig),
    Disconnect,
    RefreshServices(PrinterConfig),
    Shutdown,
}

#[derive(Debug)]
pub enum NetworkEvent {
    Connecting,
    Connected,
    Disconnected,
    Error(String),
    Report {
        topic: String,
        payload: Value,
    },
    ServiceProbe {
        port: u16,
        available: bool,
        latency_ms: Option<u128>,
    },
}

pub struct NetworkHandle {
    commands: mpsc::UnboundedSender<NetworkCommand>,
    pub events: mpsc::UnboundedReceiver<NetworkEvent>,
}

impl NetworkHandle {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("bambux-network".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("network runtime");
                runtime.block_on(network_main(command_rx, event_tx));
            })
            .expect("spawn network thread");

        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, command: NetworkCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for NetworkHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(NetworkCommand::Shutdown);
    }
}

async fn network_main(
    mut commands: mpsc::UnboundedReceiver<NetworkCommand>,
    events: mpsc::UnboundedSender<NetworkEvent>,
) {
    let mut pending = None;
    loop {
        let command = match pending.take() {
            Some(command) => command,
            None => match commands.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        match command {
            NetworkCommand::Connect(config) => {
                let probe_events = events.clone();
                let probe_config = config.clone();
                tokio::spawn(async move {
                    probe_services(probe_config, probe_events).await;
                });
                match run_mqtt(&config, &mut commands, &events).await {
                    MqttExit::Disconnected => {}
                    MqttExit::Replace(config) => {
                        pending = Some(NetworkCommand::Connect(config));
                    }
                    MqttExit::Shutdown => break,
                }
            }
            NetworkCommand::RefreshServices(config) => {
                probe_services(config, events.clone()).await;
            }
            NetworkCommand::Disconnect => {
                emit(&events, NetworkEvent::Disconnected);
            }
            NetworkCommand::Shutdown => break,
        }
    }
}

enum MqttExit {
    Disconnected,
    Replace(PrinterConfig),
    Shutdown,
}

async fn run_mqtt(
    config: &PrinterConfig,
    commands: &mut mpsc::UnboundedReceiver<NetworkCommand>,
    events: &mpsc::UnboundedSender<NetworkEvent>,
) -> MqttExit {
    emit(events, NetworkEvent::Connecting);

    let host = config.normalized_host();
    let client_id = format!("bambux-{}", std::process::id());
    let mut options = MqttOptions::new(client_id, host, 8883);
    options.set_keep_alive(Duration::from_secs(10));
    options.set_clean_session(true);
    options.set_max_packet_size(MAX_INCOMING_PACKET_SIZE, MAX_OUTGOING_PACKET_SIZE);
    options.set_credentials("bblp", config.access_code.clone());
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Rustls(
        Arc::new(insecure_bambu_tls_config()),
    )));

    let (client, mut event_loop) = AsyncClient::new(options, 64);
    let mut announced_connection = false;
    let mut requested_serial: Option<String> = None;

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(NetworkCommand::Connect(next)) => {
                        let _ = client.disconnect().await;
                        return MqttExit::Replace(next);
                    }
                    Some(NetworkCommand::Disconnect) => {
                        let _ = client.disconnect().await;
                        emit(events, NetworkEvent::Disconnected);
                        return MqttExit::Disconnected;
                    }
                    Some(NetworkCommand::RefreshServices(config)) => {
                        let probe_events = events.clone();
                        tokio::spawn(async move {
                            probe_services(config, probe_events).await;
                        });
                    }
                    Some(NetworkCommand::Shutdown) | None => {
                        let _ = client.disconnect().await;
                        return MqttExit::Shutdown;
                    }
                }
            }
            polled = event_loop.poll() => {
                match polled {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        requested_serial = None;
                        match client.subscribe("device/+/report", QoS::AtMostOnce).await {
                            Ok(()) => {
                                if !announced_connection {
                                    announced_connection = true;
                                    emit(events, NetworkEvent::Connected);
                                }
                            }
                            Err(error) => emit(
                                events,
                                NetworkEvent::Error(format!("Could not subscribe: {error}")),
                            ),
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        if let Ok(payload) = serde_json::from_slice::<Value>(&publish.payload) {
                            if requested_serial.is_none()
                                && let Some(serial) = serial_from_report_topic(&publish.topic)
                            {
                                let request_topic = format!("device/{serial}/request");
                                let request = json!({
                                    "pushing": {
                                        "sequence_id": "0",
                                        "command": "pushall"
                                    }
                                });
                                let _ = client.publish(
                                    request_topic,
                                    QoS::AtMostOnce,
                                    false,
                                    request.to_string(),
                                ).await;
                                requested_serial = Some(serial.to_owned());
                            }
                            emit(
                                events,
                                NetworkEvent::Report {
                                    topic: publish.topic,
                                    payload,
                                },
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        announced_connection = false;
                        emit(events, NetworkEvent::Error(friendly_mqtt_error(&error.to_string())));
                        time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
    }
}

async fn probe_services(config: PrinterConfig, events: mpsc::UnboundedSender<NetworkEvent>) {
    let host = config.normalized_host();
    for port in [8883_u16, 990, 3000, 6000] {
        let started = Instant::now();
        let connected = time::timeout(
            Duration::from_millis(1600),
            TcpStream::connect((host.as_str(), port)),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        emit(
            &events,
            NetworkEvent::ServiceProbe {
                port,
                available: connected,
                latency_ms: connected.then(|| started.elapsed().as_millis()),
            },
        );
    }
}

fn emit(events: &mpsc::UnboundedSender<NetworkEvent>, event: NetworkEvent) {
    let _ = events.send(event);
}

fn serial_from_report_topic(topic: &str) -> Option<&str> {
    let mut parts = topic.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("device"), Some(serial), Some("report")) => Some(serial),
        _ => None,
    }
}

fn friendly_mqtt_error(error: &str) -> String {
    if error.to_ascii_lowercase().contains("connection refused") {
        "Printer refused MQTT authentication. Check the access code.".into()
    } else {
        format!("MQTT connection lost: {error}")
    }
}

/// Bambu printers use a device-generated, self-signed certificate on the LAN.
/// TLS still encrypts the access code and telemetry, but certificate identity
/// cannot be validated against the public WebPKI. Keep the printer on a trusted
/// private network and never forward port 8883 to the internet.
fn insecure_bambu_tls_config() -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(BambuCertificateVerifier))
        .with_no_client_auth()
}

#[derive(Debug)]
struct BambuCertificateVerifier;

impl ServerCertVerifier for BambuCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

impl fmt::Display for NetworkEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connecting => f.write_str("connecting"),
            Self::Connected => f.write_str("connected"),
            Self::Disconnected => f.write_str("disconnected"),
            Self::Error(message) => f.write_str(message),
            Self::Report { .. } => f.write_str("report"),
            Self::ServiceProbe { port, .. } => write!(f, "port {port}"),
        }
    }
}
