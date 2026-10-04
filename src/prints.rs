//! Print history assembled from the printer's SD card: sliced projects in the
//! root directory, paired with the timelapse videos the printer recorded.

use std::{collections::HashMap, io::Read, sync::Arc};

use anyhow::{Context as _, Result};
use chrono::{DateTime, NaiveDateTime, Utc};

/// A sliced project file on the SD card.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub path: String,
    pub size: usize,
    /// Upload time from `MDTM` (UTC), which is when the print was sent.
    pub modified: DateTime<Utc>,
}

/// A timelapse recording in `/timelapse`.
#[derive(Debug, Clone)]
pub struct Timelapse {
    pub video_path: String,
    pub thumbnail_path: String,
    pub size: usize,
    /// Timestamp encoded in the file name. The printer writes it in its own
    /// display timezone, so it is offset from UTC by an unknown amount.
    pub named_at: NaiveDateTime,
    /// Last write (`MDTM`, UTC), which is when the print finished.
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct PrintRecord {
    /// Stable identifier: the project path, or the video path when the
    /// project file is gone.
    pub key: String,
    pub name: String,
    pub started_at: DateTime<Utc>,
    pub file: Option<RemoteFile>,
    pub timelapse: Option<Timelapse>,
}

impl PrintRecord {
    /// Wall-clock print time, known when a timelapse marks the finish.
    pub fn duration_secs(&self) -> Option<i64> {
        let finished = self.timelapse.as_ref()?.finished_at?;
        let secs = (finished - self.started_at).num_seconds();
        (secs > 0).then_some(secs)
    }
}

/// Slicer metadata read from a project's `Metadata/` folder.
#[derive(Debug, Clone, Default)]
pub struct PrintDetails {
    pub predicted_secs: Option<u32>,
    pub weight_g: Option<f32>,
    pub filaments: Vec<Filament>,
    /// The slicer's plate render (`Metadata/plate_N.png`).
    pub plate_png: Option<Arc<Vec<u8>>>,
}

#[derive(Debug, Clone)]
pub struct Filament {
    pub kind: String,
    /// `#RRGGBB`.
    pub color: String,
    pub used_g: Option<f32>,
}

/// Parse `video_YYYY-MM-DD_HH-MM-SS.mp4`.
pub fn parse_video_name(name: &str) -> Option<NaiveDateTime> {
    let stamp = name.strip_prefix("video_")?.strip_suffix(".mp4")?;
    NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d_%H-%M-%S").ok()
}

/// Display name for a project path: the file name without `.gcode.3mf`/`.3mf`.
pub fn project_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".gcode.3mf")
        .or_else(|| file.strip_suffix(".3mf"))
        .unwrap_or(file)
        .to_owned()
}

/// Largest gap between a project's upload and its timelapse starting.
const MATCH_TOLERANCE_SECS: i64 = 5 * 60;

/// Pair projects with timelapses and return the history, newest first.
///
/// Video names use the printer's display timezone, so first learn that
/// offset: it is the quarter-hour shift that lines up the most
/// (video name, upload time) pairs to within a couple of minutes.
pub fn assemble(files: Vec<RemoteFile>, videos: Vec<Timelapse>) -> Vec<PrintRecord> {
    let offset = timezone_offset(&files, &videos);

    let mut candidates: Vec<(i64, usize, usize)> = Vec::new();
    for (video_index, video) in videos.iter().enumerate() {
        let started = video.named_at.and_utc().timestamp() - offset;
        for (file_index, file) in files.iter().enumerate() {
            let gap = (started - file.modified.timestamp()).abs();
            if gap <= MATCH_TOLERANCE_SECS {
                candidates.push((gap, video_index, file_index));
            }
        }
    }
    // Greedy closest-first so each video and file is used at most once.
    candidates.sort_unstable();
    let mut video_for_file: HashMap<usize, usize> = HashMap::new();
    let mut taken_videos = vec![false; videos.len()];
    for (_, video_index, file_index) in candidates {
        if taken_videos[video_index] || video_for_file.contains_key(&file_index) {
            continue;
        }
        taken_videos[video_index] = true;
        video_for_file.insert(file_index, video_index);
    }

    let mut records: Vec<PrintRecord> = files
        .iter()
        .enumerate()
        .map(|(index, file)| PrintRecord {
            key: file.path.clone(),
            name: project_name(&file.path),
            started_at: file.modified,
            file: Some(file.clone()),
            timelapse: video_for_file.get(&index).map(|&v| videos[v].clone()),
        })
        .collect();
    // Timelapses whose project was deleted still record a print.
    records.extend(
        videos
            .iter()
            .zip(&taken_videos)
            .filter(|(_, taken)| !**taken)
            .map(|(video, _)| PrintRecord {
                key: video.video_path.clone(),
                name: "Untitled print".into(),
                started_at: DateTime::from_timestamp(
                    video.named_at.and_utc().timestamp() - offset,
                    0,
                )
                .unwrap_or_default(),
                file: None,
                timelapse: Some(video.clone()),
            }),
    );
    records.sort_by_key(|record| std::cmp::Reverse(record.started_at));
    records
}

fn timezone_offset(files: &[RemoteFile], videos: &[Timelapse]) -> i64 {
    const STEP: i64 = 15 * 60;
    let mut votes: HashMap<i64, usize> = HashMap::new();
    for video in videos {
        let named = video.named_at.and_utc().timestamp();
        for file in files {
            let delta = named - file.modified.timestamp();
            let rounded = (delta as f64 / STEP as f64).round() as i64 * STEP;
            if (delta - rounded).abs() <= 120 && rounded.abs() <= 14 * 3600 {
                *votes.entry(rounded).or_default() += 1;
            }
        }
    }
    votes
        .into_iter()
        .max_by_key(|&(offset, count)| (count, -offset.abs()))
        .map_or(0, |(offset, _)| offset)
}

/// Read slicer metadata and the plate render from a project archive.
pub fn read_details(bytes: &[u8]) -> Result<PrintDetails> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("invalid 3MF ZIP container")?;
    let mut details = PrintDetails::default();

    if let Ok(mut entry) = archive.by_name("Metadata/slice_info.config") {
        let mut xml = String::new();
        entry
            .by_ref()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut xml)?;
        parse_slice_info(&xml, &mut details)?;
    }

    // Prefer the lowest-numbered plate render.
    let plate = (0..archive.len())
        .filter_map(|index| {
            let entry = archive.by_index(index).ok()?;
            let number = entry
                .name()
                .strip_prefix("Metadata/plate_")?
                .strip_suffix(".png")?
                .parse::<u32>()
                .ok()?;
            Some((number, index))
        })
        .min();
    if let Some((_, index)) = plate {
        let mut entry = archive.by_index(index)?;
        let mut png = Vec::with_capacity(entry.size() as usize);
        entry
            .by_ref()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut png)?;
        details.plate_png = Some(Arc::new(png));
    }
    Ok(details)
}

fn parse_slice_info(xml: &str, details: &mut PrintDetails) -> Result<()> {
    let document = roxmltree::Document::parse(xml).context("invalid slice_info.config")?;
    let Some(plate) = document
        .descendants()
        .find(|node| node.has_tag_name("plate"))
    else {
        return Ok(());
    };
    for node in plate.children().filter(|node| node.is_element()) {
        match node.tag_name().name() {
            "metadata" => match node.attribute("key") {
                Some("prediction") => {
                    details.predicted_secs = node.attribute("value").and_then(|v| v.parse().ok());
                }
                Some("weight") => {
                    details.weight_g = node.attribute("value").and_then(|v| v.parse().ok());
                }
                _ => {}
            },
            "filament" => details.filaments.push(Filament {
                kind: node.attribute("type").unwrap_or("?").to_owned(),
                color: node.attribute("color").unwrap_or("#888888").to_owned(),
                used_g: node.attribute("used_g").and_then(|v| v.parse().ok()),
            }),
            _ => {}
        }
    }
    Ok(())
}

/// `1h 02m`, `14m`, `45s`.
pub fn format_duration(secs: i64) -> String {
    let (hours, minutes) = (secs / 3600, (secs % 3600) / 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn file(path: &str, at: &str) -> RemoteFile {
        RemoteFile {
            path: path.into(),
            size: 1,
            modified: Utc.from_utc_datetime(
                &NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S").unwrap(),
            ),
        }
    }

    fn video(name: &str) -> Timelapse {
        Timelapse {
            video_path: format!("/timelapse/{name}"),
            thumbnail_path: String::new(),
            size: 1,
            named_at: parse_video_name(name).unwrap(),
            finished_at: None,
        }
    }

    #[test]
    fn pairs_videos_across_the_printer_timezone() {
        // Real values from a printer whose video names run UTC+8.
        let records = assemble(
            vec![
                file("/rohan_spinning_top.gcode.3mf", "2026-10-04 18:44:00"),
                file("/face_glasses_holder.gcode.3mf", "2026-09-27 15:45:14"),
                file("/toothbrush v3.gcode.3mf", "2026-01-25 19:17:40"),
                file("/never_recorded.gcode.3mf", "2026-08-01 12:00:00"),
            ],
            vec![
                video("video_2026-10-05_02-44-02.mp4"),
                video("video_2026-09-27_23-45-17.mp4"),
                video("video_2026-01-26_03-17-43.mp4"),
                video("video_2026-06-15_00-04-36.mp4"),
            ],
        );
        let by_name: HashMap<_, _> = records
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.timelapse.as_ref().map(|t| t.video_path.as_str()),
                )
            })
            .collect();
        assert_eq!(
            by_name["rohan_spinning_top"],
            Some("/timelapse/video_2026-10-05_02-44-02.mp4")
        );
        assert_eq!(
            by_name["toothbrush v3"],
            Some("/timelapse/video_2026-01-26_03-17-43.mp4")
        );
        assert_eq!(by_name["never_recorded"], None);
        // The orphan video becomes its own record, dated in UTC.
        let orphan = records.iter().find(|r| r.file.is_none()).unwrap();
        assert_eq!(orphan.started_at.to_rfc3339(), "2026-06-14T16:04:36+00:00");
        assert_eq!(records[0].name, "rohan_spinning_top");
    }

    #[test]
    fn reads_slice_info() {
        let xml = r##"<config><plate><metadata key="prediction" value="3673"/><metadata key="weight" value="46.00"/><filament id="1" type="PLA" color="#FFFFFF" used_g="46.00"/></plate></config>"##;
        let mut details = PrintDetails::default();
        parse_slice_info(xml, &mut details).unwrap();
        assert_eq!(details.predicted_secs, Some(3673));
        assert_eq!(details.weight_g, Some(46.0));
        assert_eq!(details.filaments[0].kind, "PLA");
        assert_eq!(format_duration(3673), "1h 01m");
    }
}
