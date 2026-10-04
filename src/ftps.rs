use std::{
    net::{TcpStream, ToSocketAddrs},
    str::FromStr,
    sync::{Arc, mpsc},
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use rustls_ftps::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, ring},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use suppaftp::{
    FtpError, RustlsConnector, RustlsFtpStream, Status, list::File as ListedFile, types::FileType,
};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

use crate::{
    model::PrinterConfig,
    model_3mf::{ModelMesh, parse_3mf},
};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FtpsEntry {
    pub name: String,
    pub is_directory: bool,
    pub size: usize,
    pub modified: Option<SystemTime>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum FtpsCommand {
    List {
        config: PrinterConfig,
        path: String,
    },
    LoadModel {
        config: PrinterConfig,
        remote_path: String,
    },
    /// Fetch the model for the job the printer reports, by project name.
    LoadJob {
        config: PrinterConfig,
        job_name: String,
    },
    Shutdown,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum FtpsEvent {
    LoadingDirectory(String),
    Listing {
        path: String,
        entries: Vec<FtpsEntry>,
    },
    LoadingModel(String),
    ModelLoaded {
        remote_path: String,
        model: ModelMesh,
    },
    JobModelLoaded {
        job_name: String,
        model: ModelMesh,
    },
    JobModelUnavailable {
        job_name: String,
        error: String,
    },
    Error(String),
}

#[allow(dead_code)]
pub struct FtpsHandle {
    commands: mpsc::Sender<FtpsCommand>,
    pub events: mpsc::Receiver<FtpsEvent>,
}

#[allow(dead_code, unused_variables)]
impl FtpsHandle {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("bambux-ftps".into())
            .spawn(move || worker(command_rx, event_tx))
            .expect("spawn FTPS worker");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, command: FtpsCommand) {
        let _ = self.commands.send(command);
    }
}

impl Drop for FtpsHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(FtpsCommand::Shutdown);
    }
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
fn worker(commands: mpsc::Receiver<FtpsCommand>, events: mpsc::Sender<FtpsEvent>) {
    while let Ok(command) = commands.recv() {
        match command {
            FtpsCommand::List { config, path } => {
                let _ = events.send(FtpsEvent::LoadingDirectory(path.clone()));
                match list_directory(&config, &path) {
                    Ok(entries) => {
                        let _ = events.send(FtpsEvent::Listing { path, entries });
                    }
                    Err(error) => {
                        let _ =
                            events.send(FtpsEvent::Error(format!("FTPS browse failed: {error:#}")));
                    }
                }
            }
            FtpsCommand::LoadModel {
                config,
                remote_path,
            } => {
                let _ = events.send(FtpsEvent::LoadingModel(remote_path.clone()));
                match download(&config, &remote_path).and_then(|bytes| {
                    let name = remote_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(&remote_path)
                        .to_owned();
                    parse_3mf(name, bytes)
                }) {
                    Ok(model) => {
                        let _ = events.send(FtpsEvent::ModelLoaded { remote_path, model });
                    }
                    Err(error) => {
                        let _ =
                            events.send(FtpsEvent::Error(format!("3MF preview failed: {error:#}")));
                    }
                }
            }
            FtpsCommand::LoadJob { config, job_name } => {
                let candidates = job_paths(&job_name);
                match download_first(&config, &candidates)
                    .and_then(|(path, bytes)| parse_3mf(path, bytes))
                {
                    Ok(model) => {
                        let _ = events.send(FtpsEvent::JobModelLoaded { job_name, model });
                    }
                    Err(error) => {
                        let _ = events.send(FtpsEvent::JobModelUnavailable {
                            job_name,
                            error: format!("{error:#}"),
                        });
                    }
                }
            }
            FtpsCommand::Shutdown => break,
        }
    }
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
fn connect(config: &PrinterConfig) -> anyhow::Result<RustlsFtpStream> {
    let host = config.normalized_host();
    let addr = (host.as_str(), 990)
        .to_socket_addrs()?
        .next()
        .with_context(|| format!("could not resolve {host}"))?;
    // Fail fast when the printer is unreachable rather than waiting on the OS connect timeout.
    TcpStream::connect_timeout(&addr, IO_TIMEOUT).context("connect")?;
    // Printer vsftpd requires data connections to resume the control connection's TLS
    // session (`require_ssl_reuse`). rustls caches sessions in the shared ClientConfig, so
    // every data channel opened through this connector resumes the control session.
    let mut ftp = RustlsFtpStream::connect_secure_implicit(
        addr,
        RustlsConnector::from(Arc::new(insecure_tls_config()?)),
        &host,
    )?;
    ftp.get_ref().set_read_timeout(Some(IO_TIMEOUT))?;
    ftp.get_ref().set_write_timeout(Some(IO_TIMEOUT))?;
    // Data streams are dialed by suppaftp; give them the same timeouts so a stalled
    // transfer surfaces as an error instead of wedging the worker thread.
    ftp = ftp.passive_stream_builder(|addr| {
        let stream =
            TcpStream::connect_timeout(&addr, IO_TIMEOUT).map_err(FtpError::ConnectionError)?;
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .map_err(FtpError::ConnectionError)?;
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(FtpError::ConnectionError)?;
        Ok(stream)
    });
    ftp.login("bblp", &config.access_code).context("login")?;
    // Implicit-TLS connect skips PBSZ/PROT, but suppaftp always wraps the data channel in
    // TLS. Without PROT P, vsftpd sends the data channel in cleartext and the handshake stalls.
    ftp.custom_command("PBSZ 0", &[Status::CommandOk])
        .context("PBSZ")?;
    ftp.custom_command("PROT P", &[Status::CommandOk])
        .context("PROT")?;
    ftp.transfer_type(FileType::Binary)?;
    Ok(ftp)
}

/// The printer presents a device-generated certificate that can't be validated against
/// WebPKI; see `network::insecure_bambu_tls_config` for the same trade-off on MQTT.
fn insecure_tls_config() -> anyhow::Result<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    Ok(ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
        .with_no_client_auth())
}

#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls_ftps::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls_ftps::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls_ftps::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
fn list_directory(config: &PrinterConfig, path: &str) -> anyhow::Result<Vec<FtpsEntry>> {
    let mut ftp = connect(config)?;
    ftp.cwd(path)?;
    let lines = ftp.list(None)?;
    let mut entries: Vec<_> = lines
        .into_iter()
        .filter_map(|line| {
            if let Ok(file) = ListedFile::from_str(&line) {
                if matches!(file.name(), "." | "..") {
                    return None;
                }
                Some(FtpsEntry {
                    name: file.name().to_owned(),
                    is_directory: file.is_directory(),
                    size: file.size(),
                    modified: Some(file.modified()),
                })
            } else {
                let name = line.trim().to_owned();
                (!name.is_empty()).then_some(FtpsEntry {
                    name,
                    is_directory: false,
                    size: 0,
                    modified: None,
                })
            }
        })
        .collect();
    entries.sort_by(|left, right| {
        right.is_directory.cmp(&left.is_directory).then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
    });
    let _ = ftp.quit();
    Ok(entries)
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
fn download(config: &PrinterConfig, remote_path: &str) -> anyhow::Result<Vec<u8>> {
    let mut ftp = connect(config)?;
    let bytes = ftp.retr_as_buffer(remote_path)?.into_inner();
    let _ = ftp.quit();
    Ok(bytes)
}

/// Where Bambu Studio leaves a job's sliced project on the SD card.
fn job_paths(job_name: &str) -> Vec<String> {
    let name = job_name.trim().trim_start_matches('/');
    let file = if name.ends_with(".3mf") {
        name.to_owned()
    } else {
        format!("{name}.gcode.3mf")
    };
    vec![format!("/{file}"), format!("/cache/{file}")]
}

/// Download the first of `candidates` that exists, over a single session.
fn download_first(
    config: &PrinterConfig,
    candidates: &[String],
) -> anyhow::Result<(String, Vec<u8>)> {
    let mut ftp = connect(config)?;
    let mut last_error = None;
    for path in candidates {
        match ftp.retr_as_buffer(path) {
            Ok(buffer) => {
                let _ = ftp.quit();
                return Ok((path.clone(), buffer.into_inner()));
            }
            Err(error) => last_error = Some(error),
        }
    }
    let _ = ftp.quit();
    Err(anyhow::anyhow!(
        "job file not found ({})",
        last_error.map_or_else(|| "no candidates".into(), |error| error.to_string())
    ))
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
pub fn join_path(parent: &str, child: &str) -> String {
    if parent == "/" {
        format!("/{child}")
    } else {
        format!("{}/{child}", parent.trim_end_matches('/'))
    }
}

#[allow(dead_code, unused_variables)]
#[allow(unfulfilled_lint_expectations)]
pub fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((parent, _)) => parent.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_are_normalized() {
        assert_eq!(join_path("/", "cache"), "/cache");
        assert_eq!(join_path("/cache", "part.3mf"), "/cache/part.3mf");
        assert_eq!(parent_path("/cache/part.3mf"), "/cache");
        assert_eq!(parent_path("/cache"), "/");
    }

    #[test]
    fn job_paths_cover_root_and_cache() {
        assert_eq!(
            job_paths("rohan_spinning_top"),
            [
                "/rohan_spinning_top.gcode.3mf",
                "/cache/rohan_spinning_top.gcode.3mf"
            ]
        );
    }
}
