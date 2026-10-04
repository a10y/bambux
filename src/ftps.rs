use std::{
    collections::VecDeque,
    fs,
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, mpsc},
    time::Duration,
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

use crate::{
    model::PrinterConfig,
    model_3mf::{ModelMesh, parse_3mf},
    prints::{self, PrintDetails, PrintRecord, RemoteFile, Timelapse},
};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum FtpsCommand {
    /// List projects and timelapses, then prefetch their metadata in the background.
    ScanPrints {
        config: PrinterConfig,
    },
    LoadPrintModel {
        config: PrinterConfig,
        key: String,
        file: RemoteFile,
    },
    FetchTimelapse {
        config: PrinterConfig,
        key: String,
        timelapse: Timelapse,
    },
    /// Fetch the model for the job the printer reports, by project name.
    LoadJob {
        config: PrinterConfig,
        job_name: String,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum FtpsEvent {
    PrintsScanned(Vec<PrintRecord>),
    ScanFailed(String),
    PrintDetails { key: String, details: PrintDetails },
    TimelapseThumbnail { key: String, jpeg: Arc<Vec<u8>> },
    PrintModelLoaded { key: String, model: ModelMesh },
    PrintModelFailed { key: String, error: String },
    TimelapseReady { key: String, path: PathBuf },
    TimelapseFailed { key: String, error: String },
    JobModelLoaded { job_name: String, model: ModelMesh },
    JobModelUnavailable { job_name: String, error: String },
}

pub struct FtpsHandle {
    commands: mpsc::Sender<FtpsCommand>,
    pub events: mpsc::Receiver<FtpsEvent>,
}

impl FtpsHandle {
    /// `cache_dir` holds downloaded projects and videos between runs.
    pub fn spawn(cache_dir: PathBuf) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("bambux-ftps".into())
            .spawn(move || {
                Worker {
                    events: event_tx,
                    cache_dir,
                    session: None,
                }
                .run(command_rx)
            })
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

/// Background work queued after a scan; runs only while no command is waiting.
enum Prefetch {
    Thumbnail {
        config: PrinterConfig,
        key: String,
        path: String,
    },
    Details {
        config: PrinterConfig,
        key: String,
        file: RemoteFile,
    },
}

struct Worker {
    events: mpsc::Sender<FtpsEvent>,
    cache_dir: PathBuf,
    /// One control session reused across operations; reconnected on failure.
    session: Option<(PrinterConfig, RustlsFtpStream)>,
}

impl Worker {
    fn run(mut self, commands: mpsc::Receiver<FtpsCommand>) {
        let mut prefetch = VecDeque::new();
        loop {
            // Interactive commands always pre-empt queued prefetch work.
            let command = match commands.try_recv() {
                Ok(command) => command,
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {
                    if let Some(job) = prefetch.pop_front() {
                        self.prefetch(job);
                        continue;
                    }
                    match commands.recv() {
                        Ok(command) => command,
                        Err(_) => break,
                    }
                }
            };
            match command {
                FtpsCommand::ScanPrints { config } => {
                    prefetch.clear();
                    match self.scan(&config) {
                        Ok(records) => {
                            prefetch.extend(records.iter().filter_map(|record| {
                                Some(Prefetch::Thumbnail {
                                    config: config.clone(),
                                    key: record.key.clone(),
                                    path: record.timelapse.as_ref()?.thumbnail_path.clone(),
                                })
                            }));
                            prefetch.extend(records.iter().filter_map(|record| {
                                Some(Prefetch::Details {
                                    config: config.clone(),
                                    key: record.key.clone(),
                                    file: record.file.clone()?,
                                })
                            }));
                            self.emit(FtpsEvent::PrintsScanned(records));
                        }
                        Err(error) => self.emit(FtpsEvent::ScanFailed(format!("{error:#}"))),
                    }
                }
                FtpsCommand::LoadPrintModel { config, key, file } => {
                    let result = self
                        .fetch_bytes(
                            &config,
                            &file.path,
                            Some(file.size),
                            file.modified.timestamp(),
                        )
                        .and_then(|bytes| parse_3mf(prints::project_name(&file.path), bytes));
                    self.emit(match result {
                        Ok(model) => FtpsEvent::PrintModelLoaded { key, model },
                        Err(error) => FtpsEvent::PrintModelFailed {
                            key,
                            error: format!("{error:#}"),
                        },
                    });
                }
                FtpsCommand::FetchTimelapse {
                    config,
                    key,
                    timelapse,
                } => {
                    let stamp = timelapse.named_at.and_utc().timestamp();
                    let result = self.fetch_file(
                        &config,
                        &timelapse.video_path,
                        Some(timelapse.size),
                        stamp,
                    );
                    self.emit(match result {
                        Ok(path) => FtpsEvent::TimelapseReady { key, path },
                        Err(error) => FtpsEvent::TimelapseFailed {
                            key,
                            error: format!("{error:#}"),
                        },
                    });
                }
                FtpsCommand::LoadJob { config, job_name } => {
                    let result = self
                        .download_first(&config, &job_paths(&job_name))
                        .and_then(|(path, bytes)| parse_3mf(path, bytes));
                    self.emit(match result {
                        Ok(model) => FtpsEvent::JobModelLoaded { job_name, model },
                        Err(error) => FtpsEvent::JobModelUnavailable {
                            job_name,
                            error: format!("{error:#}"),
                        },
                    });
                }
                FtpsCommand::Shutdown => break,
            }
        }
        if let Some((_, mut ftp)) = self.session.take() {
            let _ = ftp.quit();
        }
    }

    fn emit(&self, event: FtpsEvent) {
        let _ = self.events.send(event);
    }

    fn prefetch(&mut self, job: Prefetch) {
        match job {
            Prefetch::Thumbnail { config, key, path } => {
                if let Ok(bytes) = self.fetch_bytes(&config, &path, None, 0) {
                    self.emit(FtpsEvent::TimelapseThumbnail {
                        key,
                        jpeg: Arc::new(bytes),
                    });
                }
            }
            Prefetch::Details { config, key, file } => {
                let details = self
                    .fetch_bytes(
                        &config,
                        &file.path,
                        Some(file.size),
                        file.modified.timestamp(),
                    )
                    .and_then(|bytes| prints::read_details(&bytes));
                match details {
                    Ok(details) => self.emit(FtpsEvent::PrintDetails { key, details }),
                    Err(error) => tracing::warn!("no details for {}: {error:#}", file.path),
                }
            }
        }
    }

    /// Run `op` on the shared session, reconnecting once if a reused session
    /// turns out to be stale.
    fn with_ftp<T>(
        &mut self,
        config: &PrinterConfig,
        mut op: impl FnMut(&mut RustlsFtpStream) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        loop {
            let reused = matches!(&self.session, Some((current, _)) if current == config);
            if !reused {
                if let Some((_, mut old)) = self.session.take() {
                    let _ = old.quit();
                }
                self.session = Some((config.clone(), connect(config)?));
            }
            let (_, ftp) = self.session.as_mut().expect("session just ensured");
            match op(ftp) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    self.session = None;
                    if !reused {
                        return Err(error);
                    }
                }
            }
        }
    }

    fn scan(&mut self, config: &PrinterConfig) -> anyhow::Result<Vec<PrintRecord>> {
        self.with_ftp(config, |ftp| {
            let mut files = Vec::new();
            for entry in list(ftp, "/")? {
                if entry.is_directory() || !entry.name().to_ascii_lowercase().ends_with(".3mf") {
                    continue;
                }
                let path = format!("/{}", entry.name());
                let modified = ftp.mdtm(&path).with_context(|| format!("MDTM {path}"))?;
                files.push(RemoteFile {
                    path,
                    size: entry.size(),
                    modified: modified.and_utc(),
                });
            }
            // Timelapse recording may never have been enabled.
            let mut videos = Vec::new();
            for entry in list(ftp, "/timelapse").unwrap_or_default() {
                let Some(named_at) = prints::parse_video_name(entry.name()) else {
                    continue;
                };
                let video_path = format!("/timelapse/{}", entry.name());
                let stem = entry.name().trim_end_matches(".mp4");
                videos.push(Timelapse {
                    finished_at: ftp.mdtm(&video_path).ok().map(|at| at.and_utc()),
                    thumbnail_path: format!("/timelapse/thumbnail/{stem}.jpg"),
                    video_path,
                    size: entry.size(),
                    named_at,
                });
            }
            Ok(prints::assemble(files, videos))
        })
    }

    /// Download `remote` into the cache unless an intact copy is already there.
    /// `stamp` (a modification time) keys the cache so edits are refetched.
    fn fetch_file(
        &mut self,
        config: &PrinterConfig,
        remote: &str,
        size: Option<usize>,
        stamp: i64,
    ) -> anyhow::Result<PathBuf> {
        let local = cache_path(&self.cache_dir, remote, stamp);
        let cached = fs::metadata(&local)
            .is_ok_and(|meta| size.is_none_or(|size| meta.len() == size as u64));
        if cached {
            return Ok(local);
        }
        let bytes = self.with_ftp(config, |ftp| {
            Ok(ftp
                .retr_as_buffer(remote)
                .with_context(|| format!("download {remote}"))?
                .into_inner())
        })?;
        if let Some(parent) = local.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write then rename so an interrupted download never looks complete.
        let partial = local.with_extension("partial");
        fs::write(&partial, &bytes)?;
        fs::rename(&partial, &local)?;
        Ok(local)
    }

    fn fetch_bytes(
        &mut self,
        config: &PrinterConfig,
        remote: &str,
        size: Option<usize>,
        stamp: i64,
    ) -> anyhow::Result<Vec<u8>> {
        let path = self.fetch_file(config, remote, size, stamp)?;
        Ok(fs::read(path)?)
    }

    /// Download the first of `candidates` that exists.
    fn download_first(
        &mut self,
        config: &PrinterConfig,
        candidates: &[String],
    ) -> anyhow::Result<(String, Vec<u8>)> {
        for path in candidates {
            let found = self.with_ftp(config, |ftp| match ftp.retr_as_buffer(path) {
                Ok(buffer) => Ok(Some(buffer.into_inner())),
                // 550: no such file; try the next location on the same session.
                Err(FtpError::UnexpectedResponse(response))
                    if response.status == Status::FileUnavailable =>
                {
                    Ok(None)
                }
                Err(error) => Err(error.into()),
            })?;
            if let Some(bytes) = found {
                return Ok((path.clone(), bytes));
            }
        }
        anyhow::bail!("job file not found in {}", candidates.join(", "))
    }
}

fn list(ftp: &mut RustlsFtpStream, path: &str) -> anyhow::Result<Vec<ListedFile>> {
    let lines = ftp
        .list(Some(path))
        .with_context(|| format!("LIST {path}"))?;
    Ok(lines
        .iter()
        .filter_map(|line| ListedFile::from_str(line).ok())
        .filter(|file| !matches!(file.name(), "." | ".."))
        .collect())
}

fn cache_path(cache_dir: &Path, remote: &str, stamp: i64) -> PathBuf {
    let flat: String = remote
        .trim_start_matches('/')
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    cache_dir.join("sd").join(format!("{stamp}-{flat}"))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_are_flat_and_stamped() {
        assert_eq!(
            cache_path(Path::new("/c"), "/timelapse/thumbnail/a b.jpg", 7),
            Path::new("/c/sd/7-timelapse_thumbnail_a b.jpg")
        );
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
