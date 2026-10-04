//! Timelapse playback. The printer records H.264 High profile, which the
//! pure-Rust decoders can't handle, so frames come from an `ffmpeg` child
//! process piping scaled BGRA and are paced on a background thread.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use gpui::RenderImage;

/// Frames are scaled down to at most this width before display.
const MAX_WIDTH: u32 = 960;

#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub duration: f32,
    pub fps: f32,
    width: u32,
    height: u32,
}

#[derive(Clone, Default)]
pub struct PlayerState {
    pub frame: Option<Arc<RenderImage>>,
    pub position: f32,
    pub playing: bool,
    pub error: Option<String>,
}

enum PlayerCommand {
    Play,
    Pause,
    Seek(f32),
}

pub struct TimelapsePlayer {
    pub info: VideoInfo,
    commands: mpsc::Sender<PlayerCommand>,
    shared: Arc<Mutex<PlayerState>>,
    /// Bumped on every published frame so the UI can repaint cheaply.
    generation: Arc<AtomicU64>,
}

impl TimelapsePlayer {
    /// Open a local video and start playing it.
    pub fn open(path: &Path) -> Result<Self> {
        let ffmpeg = find_ffmpeg().ok_or_else(|| {
            anyhow!("ffmpeg is needed for timelapse playback (brew install ffmpeg)")
        })?;
        let info = probe(path)?;
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Mutex::new(PlayerState::default()));
        let generation = Arc::new(AtomicU64::new(0));
        let decoder = Decoder {
            ffmpeg,
            path: path.to_owned(),
            info,
            shared: shared.clone(),
            generation: generation.clone(),
            stream: None,
        };
        std::thread::Builder::new()
            .name("bambux-timelapse".into())
            .spawn(move || decoder.run(receiver))
            .context("spawn timelapse thread")?;
        let _ = commands.send(PlayerCommand::Play);
        Ok(Self {
            info,
            commands,
            shared,
            generation,
        })
    }

    pub fn state(&self) -> PlayerState {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn toggle(&self) {
        let playing = self.state().playing;
        let _ = self.commands.send(if playing {
            PlayerCommand::Pause
        } else {
            PlayerCommand::Play
        });
    }

    /// Jump to a fraction (0..=1) of the video.
    pub fn seek_fraction(&self, fraction: f32) {
        let _ = self.commands.send(PlayerCommand::Seek(
            fraction.clamp(0.0, 1.0) * self.info.duration,
        ));
    }
}

/// Dropping the player closes the command channel, which ends the thread.
struct Decoder {
    ffmpeg: PathBuf,
    path: PathBuf,
    info: VideoInfo,
    shared: Arc<Mutex<PlayerState>>,
    generation: Arc<AtomicU64>,
    stream: Option<Stream>,
}

struct Stream {
    child: Child,
    stdout: ChildStdout,
    start: f32,
    frames: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Decoder {
    fn run(mut self, commands: mpsc::Receiver<PlayerCommand>) {
        let frame_interval = Duration::from_secs_f32(1.0 / self.info.fps.max(1.0));
        let mut playing = false;
        let mut next_due = Instant::now();
        loop {
            let command = if playing {
                match commands.try_recv() {
                    Ok(command) => Some(command),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            } else {
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            };
            match command {
                Some(PlayerCommand::Play) => {
                    if self.stream.is_none() {
                        self.restart(0.0);
                    }
                    playing = true;
                    next_due = Instant::now();
                }
                Some(PlayerCommand::Pause) => playing = false,
                Some(PlayerCommand::Seek(position)) => {
                    self.restart(position);
                    // Show the target frame immediately, even while paused.
                    self.advance();
                    next_due = Instant::now() + frame_interval;
                }
                None => {}
            }
            if playing {
                let now = Instant::now();
                if next_due > now {
                    std::thread::sleep(next_due - now);
                }
                if !self.advance() {
                    // Timelapses are short; loop them.
                    self.restart(0.0);
                    if !self.advance() {
                        playing = false;
                    }
                }
                next_due += frame_interval;
                // Don't burst to catch up after a stall.
                if next_due + frame_interval * 4 < Instant::now() {
                    next_due = Instant::now();
                }
            }
            if let Ok(mut shared) = self.shared.lock() {
                shared.playing = playing;
            }
        }
    }

    fn restart(&mut self, position: f32) {
        self.stream = None;
        let (width, height) = output_size(self.info);
        let spawned = Command::new(&self.ffmpeg)
            .args(["-v", "error", "-ss", &format!("{position:.3}"), "-i"])
            .arg(&self.path)
            .args([
                "-an",
                "-vf",
                &format!("scale={width}:{height}"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgra",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                let stdout = child.stdout.take().expect("piped stdout");
                self.stream = Some(Stream {
                    child,
                    stdout,
                    start: position,
                    frames: 0,
                });
            }
            Err(error) => self.fail(format!("could not start ffmpeg: {error}")),
        }
    }

    /// Read and publish the next frame; false at end of stream.
    fn advance(&mut self) -> bool {
        let (width, height) = output_size(self.info);
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        let mut bgra = vec![0; (width * height * 4) as usize];
        if stream.stdout.read_exact(&mut bgra).is_err() {
            self.stream = None;
            return false;
        }
        stream.frames += 1;
        let position = stream.start + stream.frames as f32 / self.info.fps;
        let Some(buffer) = image::RgbaImage::from_raw(width, height, bgra) else {
            return false;
        };
        // gpui's RenderImage expects BGRA, which is what ffmpeg emitted.
        let frame = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        if let Ok(mut shared) = self.shared.lock() {
            shared.frame = Some(frame);
            shared.position = position.min(self.info.duration);
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn fail(&mut self, message: String) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.error = Some(message);
            shared.playing = false;
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

fn output_size(info: VideoInfo) -> (u32, u32) {
    let width = info.width.clamp(2, MAX_WIDTH) & !1;
    let height =
        ((info.height as f32 * width as f32 / info.width.max(1) as f32) as u32).max(2) & !1;
    (width, height)
}

/// Read dimensions, frame rate, and duration from the MP4 header.
fn probe(path: &Path) -> Result<VideoInfo> {
    let file = std::fs::File::open(path).context("open timelapse")?;
    let size = file.metadata()?.len();
    let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size)
        .context("invalid timelapse MP4")?;
    let track = reader
        .tracks()
        .values()
        .find(|track| track.media_type().ok() == Some(mp4::MediaType::H264))
        .ok_or_else(|| anyhow!("timelapse has no H.264 video track"))?;
    let fps = track.frame_rate() as f32;
    let fps = if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        24.0
    };
    Ok(VideoInfo {
        duration: track.duration().as_secs_f32().max(1.0 / fps),
        fps,
        width: u32::from(track.width()),
        height: u32::from(track.height()),
    })
}

/// GUI apps launched from Finder don't inherit the shell PATH, so also look
/// in the usual Homebrew and system locations.
fn find_ffmpeg() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>());
    from_path
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|dir| dir.join("ffmpeg"))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_to_even_dimensions_within_max_width() {
        let info = |width, height| VideoInfo {
            duration: 1.0,
            fps: 24.0,
            width,
            height,
        };
        assert_eq!(output_size(info(1920, 1080)), (960, 540));
        assert_eq!(output_size(info(640, 360)), (640, 360));
        assert_eq!(output_size(info(1281, 721)), (960, 540));
    }
}
