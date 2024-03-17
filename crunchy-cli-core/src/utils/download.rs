use crate::utils::ffmpeg::FFmpegPreset;
use crate::utils::filter::real_dedup_vec;
use crate::utils::os::{cache_dir, is_special_file, temp_directory, temp_named_pipe, tempfile};
use crate::utils::rate_limit::RateLimiterService;
use anyhow::{bail, Result};
use chrono::NaiveTime;
use crunchyroll_rs::media::{SkipEvents, SkipEventsEvent, Subtitle, VariantData, VariantSegment};
use crunchyroll_rs::Locale;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle};
use log::{debug, warn, LevelFilter};
use regex::Regex;
use reqwest::Client;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs};
use tempfile::TempPath;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::select;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_service::Service;

#[derive(Clone, Debug)]
pub enum MergeBehavior {
    Video,
    Audio,
    Auto,
}

impl MergeBehavior {
    pub fn parse(s: &str) -> Result<MergeBehavior, String> {
        Ok(match s.to_lowercase().as_str() {
            "video" => MergeBehavior::Video,
            "audio" => MergeBehavior::Audio,
            "auto" => MergeBehavior::Auto,
            _ => return Err(format!("'{}' is not a valid merge behavior", s)),
        })
    }
}

#[derive(Clone, derive_setters::Setters)]
pub struct DownloadBuilder {
    client: Client,
    rate_limiter: Option<RateLimiterService>,
    ffmpeg_preset: FFmpegPreset,
    default_subtitle: Option<Locale>,
    output_format: Option<String>,
    audio_sort: Option<Vec<Locale>>,
    subtitle_sort: Option<Vec<Locale>>,
    force_hardsub: bool,
    download_fonts: bool,
    no_closed_caption: bool,
    threads: usize,
    ffmpeg_threads: Option<usize>,
    audio_locale_output_map: HashMap<Locale, String>,
    subtitle_locale_output_map: HashMap<Locale, String>,
}

impl DownloadBuilder {
    pub fn new(client: Client, rate_limiter: Option<RateLimiterService>) -> DownloadBuilder {
        Self {
            client,
            rate_limiter,
            ffmpeg_preset: FFmpegPreset::default(),
            default_subtitle: None,
            output_format: None,
            audio_sort: None,
            subtitle_sort: None,
            force_hardsub: false,
            download_fonts: false,
            no_closed_caption: false,
            threads: num_cpus::get(),
            ffmpeg_threads: None,
            audio_locale_output_map: HashMap::new(),
            subtitle_locale_output_map: HashMap::new(),
        }
    }

    pub fn build(self) -> Downloader {
        Downloader {
            client: self.client,
            rate_limiter: self.rate_limiter,
            ffmpeg_preset: self.ffmpeg_preset,
            default_subtitle: self.default_subtitle,
            output_format: self.output_format,
            audio_sort: self.audio_sort,
            subtitle_sort: self.subtitle_sort,

            force_hardsub: self.force_hardsub,
            download_fonts: self.download_fonts,
            no_closed_caption: self.no_closed_caption,

            download_threads: self.threads,
            ffmpeg_threads: self.ffmpeg_threads,

            formats: vec![],

            audio_locale_output_map: self.audio_locale_output_map,
            subtitle_locale_output_map: self.subtitle_locale_output_map,
        }
    }
}

struct FFmpegMeta {
    path: TempPath,
    language: Locale,
    title: String,
}

pub struct DownloadFormat {
    pub video: (VariantData, Locale),
    pub audios: Vec<(VariantData, Locale)>,
    pub subtitles: Vec<(Subtitle, bool)>,
    pub metadata: DownloadFormatMetadata,
}

pub struct DownloadFormatMetadata {
    pub skip_events: Option<SkipEvents>,
}

pub struct Downloader {
    client: Client,
    rate_limiter: Option<RateLimiterService>,

    ffmpeg_preset: FFmpegPreset,
    default_subtitle: Option<Locale>,
    output_format: Option<String>,
    audio_sort: Option<Vec<Locale>>,
    subtitle_sort: Option<Vec<Locale>>,

    force_hardsub: bool,
    download_fonts: bool,
    no_closed_caption: bool,

    download_threads: usize,
    ffmpeg_threads: Option<usize>,

    formats: Vec<DownloadFormat>,

    audio_locale_output_map: HashMap<Locale, String>,
    subtitle_locale_output_map: HashMap<Locale, String>,
}

impl Downloader {
    pub fn add_format(&mut self, format: DownloadFormat) {
        self.formats.push(format);
    }

    pub async fn download(mut self, dst: &Path) -> Result<()> {
        // `.unwrap_or_default()` here unless https://doc.rust-lang.org/stable/std/path/fn.absolute.html
        // gets stabilized as the function might throw error on weird file paths
        let required = self.check_free_space(dst).await.unwrap_or_default();
        if let Some((path, tmp_required)) = &required.0 {
            let kb = (*tmp_required as f64) / 1024.0;
            let mb = kb / 1024.0;
            let gb = mb / 1024.0;
            warn!(
                "You may have not enough disk space to store temporary files. The temp directory ({}) should have at least {}{} free space",
                path.to_string_lossy(),
                if gb < 1.0 { mb.ceil().to_string() } else { format!("{:.2}", gb) },
                if gb < 1.0 { "MB" } else { "GB" }
            )
        }
        if let Some((path, dst_required)) = &required.1 {
            let kb = (*dst_required as f64) / 1024.0;
            let mb = kb / 1024.0;
            let gb = mb / 1024.0;
            warn!(
                "You may have not enough disk space to store the output file. The directory {} should have at least {}{} free space",
                path.to_string_lossy(),
                if gb < 1.0 { mb.ceil().to_string() } else { format!("{:.2}", gb) },
                if gb < 1.0 { "MB" } else { "GB" }
            )
        }

        if let Some(audio_sort_locales) = &self.audio_sort {
            self.formats.sort_by(|a, b| {
                audio_sort_locales
                    .iter()
                    .position(|l| l == &a.video.1)
                    .cmp(&audio_sort_locales.iter().position(|l| l == &b.video.1))
            });
        }
        for format in self.formats.iter_mut() {
            if let Some(audio_sort_locales) = &self.audio_sort {
                format.audios.sort_by(|(_, a), (_, b)| {
                    audio_sort_locales
                        .iter()
                        .position(|l| l == a)
                        .cmp(&audio_sort_locales.iter().position(|l| l == b))
                })
            }
            if let Some(subtitle_sort) = &self.subtitle_sort {
                format
                    .subtitles
                    .sort_by(|(a_subtitle, a_not_cc), (b_subtitle, b_not_cc)| {
                        let ordering = subtitle_sort
                            .iter()
                            .position(|l| l == &a_subtitle.locale)
                            .cmp(&subtitle_sort.iter().position(|l| l == &b_subtitle.locale));
                        if matches!(ordering, Ordering::Equal) {
                            a_not_cc.cmp(b_not_cc).reverse()
                        } else {
                            ordering
                        }
                    })
            }
        }

        let mut videos = vec![];
        let mut audios = vec![];
        let mut subtitles = vec![];
        let mut fonts = vec![];
        let mut chapters = None;
        let mut max_len = NaiveTime::MIN;
        let mut max_frames = 0f64;
        let fmt_space = self
            .formats
            .iter()
            .flat_map(|f| {
                f.audios
                    .iter()
                    .map(|(_, locale)| format!("Downloading {} audio", locale).len())
            })
            .max()
            .unwrap();

        for (i, format) in self.formats.iter().enumerate() {
            let video_path = self
                .download_video(
                    &format.video.0,
                    format!("{:<1$}", format!("Downloading video #{}", i + 1), fmt_space),
                )
                .await?;
            for (variant_data, locale) in format.audios.iter() {
                let audio_path = self
                    .download_audio(
                        variant_data,
                        format!("{:<1$}", format!("Downloading {} audio", locale), fmt_space),
                    )
                    .await?;
                audios.push(FFmpegMeta {
                    path: audio_path,
                    language: locale.clone(),
                    title: if i == 0 {
                        locale.to_human_readable()
                    } else {
                        format!("{} [Video: #{}]", locale.to_human_readable(), i + 1)
                    },
                })
            }

            let (len, fps) = get_video_stats(&video_path)?;
            if max_len < len {
                max_len = len
            }
            let frames = len.signed_duration_since(NaiveTime::MIN).num_seconds() as f64 * fps;
            if frames > max_frames {
                max_frames = frames;
            }

            if !format.subtitles.is_empty() {
                let progress_spinner = if log::max_level() == LevelFilter::Info {
                    let progress_spinner = ProgressBar::new_spinner()
                        .with_style(
                            ProgressStyle::with_template(
                                format!(
                                    ":: {:<1$}  {{msg}} {{spinner}}",
                                    "Downloading subtitles", fmt_space
                                )
                                .as_str(),
                            )
                            .unwrap()
                            .tick_strings(&["—", "\\", "|", "/", ""]),
                        )
                        .with_finish(ProgressFinish::Abandon);
                    progress_spinner.enable_steady_tick(Duration::from_millis(100));
                    Some(progress_spinner)
                } else {
                    None
                };

                for (subtitle, not_cc) in format.subtitles.iter() {
                    if !not_cc && self.no_closed_caption {
                        continue;
                    }

                    if let Some(pb) = &progress_spinner {
                        let mut progress_message = pb.message();
                        if !progress_message.is_empty() {
                            progress_message += ", "
                        }
                        progress_message += &subtitle.locale.to_string();
                        if !not_cc {
                            progress_message += " (CC)";
                        }
                        if i != 0 {
                            progress_message += &format!(" [Video: #{}]", i + 1);
                        }
                        pb.set_message(progress_message)
                    }

                    let mut subtitle_title = subtitle.locale.to_human_readable();
                    if !not_cc {
                        subtitle_title += " (CC)"
                    }
                    if i != 0 {
                        subtitle_title += &format!(" [Video: #{}]", i + 1)
                    }

                    let subtitle_path = self.download_subtitle(subtitle.clone(), len).await?;
                    debug!(
                        "Downloaded {} subtitles{}{}",
                        subtitle.locale,
                        (!not_cc).then_some(" (cc)").unwrap_or_default(),
                        (i != 0)
                            .then_some(format!(" for video {}", i))
                            .unwrap_or_default()
                    );
                    subtitles.push(FFmpegMeta {
                        path: subtitle_path,
                        language: subtitle.locale.clone(),
                        title: subtitle_title,
                    })
                }
            }
            videos.push(FFmpegMeta {
                path: video_path,
                language: format.video.1.clone(),
                title: if self.formats.len() == 1 {
                    "Default".to_string()
                } else {
                    format!("#{}", i + 1)
                },
            });

            if let Some(skip_events) = &format.metadata.skip_events {
                let (file, path) = tempfile(".chapter")?.into_parts();
                chapters = Some((
                    (file, path),
                    [
                        skip_events.recap.as_ref().map(|e| ("Recap", e)),
                        skip_events.intro.as_ref().map(|e| ("Intro", e)),
                        skip_events.credits.as_ref().map(|e| ("Credits", e)),
                        skip_events.preview.as_ref().map(|e| ("Preview", e)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<(&str, &SkipEventsEvent)>>(),
                ));
            }
        }

        if self.download_fonts
            && !self.force_hardsub
            && dst.extension().unwrap_or_default().to_str().unwrap() == "mkv"
        {
            let mut font_names = vec![];
            for subtitle in subtitles.iter() {
                font_names.extend(get_subtitle_stats(&subtitle.path)?)
            }
            real_dedup_vec(&mut font_names);

            let progress_spinner = if log::max_level() == LevelFilter::Info {
                let progress_spinner = ProgressBar::new_spinner()
                    .with_style(
                        ProgressStyle::with_template(
                            format!(
                                ":: {:<1$}  {{msg}} {{spinner}}",
                                "Downloading fonts", fmt_space
                            )
                            .as_str(),
                        )
                        .unwrap()
                        .tick_strings(&["—", "\\", "|", "/", ""]),
                    )
                    .with_finish(ProgressFinish::Abandon);
                progress_spinner.enable_steady_tick(Duration::from_millis(100));
                Some(progress_spinner)
            } else {
                None
            };
            for font_name in font_names {
                if let Some(pb) = &progress_spinner {
                    let mut progress_message = pb.message();
                    if !progress_message.is_empty() {
                        progress_message += ", "
                    }
                    progress_message += &font_name;
                    pb.set_message(progress_message)
                }
                if let Some((font, cached)) = self.download_font(&font_name).await? {
                    if cached {
                        if let Some(pb) = &progress_spinner {
                            let mut progress_message = pb.message();
                            progress_message += " (cached)";
                            pb.set_message(progress_message)
                        }
                        debug!("Downloaded font {} (cached)", font_name);
                    } else {
                        debug!("Downloaded font {}", font_name);
                    }

                    fonts.push(font)
                }
            }
        }

        let mut input = vec![];
        let mut maps = vec![];
        let mut attachments = vec![];
        let mut metadata = vec![];

        for (i, meta) in videos.iter().enumerate() {
            input.extend(["-i".to_string(), meta.path.to_string_lossy().to_string()]);
            maps.extend(["-map".to_string(), i.to_string()]);
            metadata.extend([
                format!("-metadata:s:v:{}", i),
                format!("title={}", meta.title),
            ]);
            // the empty language metadata is created to avoid that metadata from the original track
            // is copied
            metadata.extend([format!("-metadata:s:v:{}", i), "language=".to_string()])
        }
        for (i, meta) in audios.iter().enumerate() {
            input.extend(["-i".to_string(), meta.path.to_string_lossy().to_string()]);
            maps.extend(["-map".to_string(), (i + videos.len()).to_string()]);
            metadata.extend([
                format!("-metadata:s:a:{}", i),
                format!(
                    "language={}",
                    self.audio_locale_output_map
                        .get(&meta.language)
                        .unwrap_or(&meta.language.to_string())
                ),
            ]);
            metadata.extend([
                format!("-metadata:s:a:{}", i),
                format!("title={}", meta.title),
            ]);
        }

        for (i, font) in fonts.iter().enumerate() {
            attachments.extend(["-attach".to_string(), font.to_string_lossy().to_string()]);
            metadata.extend([
                format!("-metadata:s:t:{}", i),
                "mimetype=font/woff2".to_string(),
            ])
        }

        // this formats are supporting embedding subtitles into the video container instead of
        // burning it into the video stream directly
        let container_supports_softsubs = !self.force_hardsub
            && ["mkv", "mov", "mp4"]
                .contains(&dst.extension().unwrap_or_default().to_str().unwrap());

        if container_supports_softsubs {
            for (i, meta) in subtitles.iter().enumerate() {
                input.extend(["-i".to_string(), meta.path.to_string_lossy().to_string()]);
                maps.extend([
                    "-map".to_string(),
                    (i + videos.len() + audios.len()).to_string(),
                ]);
                metadata.extend([
                    format!("-metadata:s:s:{}", i),
                    format!(
                        "language={}",
                        self.subtitle_locale_output_map
                            .get(&meta.language)
                            .unwrap_or(&meta.language.to_string())
                    ),
                ]);
                metadata.extend([
                    format!("-metadata:s:s:{}", i),
                    format!("title={}", meta.title),
                ]);
            }
        }

        if let Some(((file, path), chapters)) = chapters.as_mut() {
            write_ffmpeg_chapters(file, max_len, chapters)?;
            input.extend(["-i".to_string(), path.to_string_lossy().to_string()]);
            maps.extend([
                "-map_metadata".to_string(),
                (videos.len()
                    + audios.len()
                    + container_supports_softsubs
                        .then_some(subtitles.len())
                        .unwrap_or_default())
                .to_string(),
            ])
        }

        let preset_custom = matches!(self.ffmpeg_preset, FFmpegPreset::Custom(_));
        let (input_presets, mut output_presets) = self.ffmpeg_preset.into_input_output_args();
        let fifo = temp_named_pipe()?;

        let mut command_args = vec![
            "-y".to_string(),
            "-hide_banner".to_string(),
            "-vstats_file".to_string(),
            fifo.path().to_string_lossy().to_string(),
        ];
        command_args.extend(input_presets);
        command_args.extend(input);
        command_args.extend(maps);
        command_args.extend(attachments);
        command_args.extend(metadata);
        if !preset_custom {
            if let Some(ffmpeg_threads) = self.ffmpeg_threads {
                command_args.extend(vec!["-threads".to_string(), ffmpeg_threads.to_string()])
            }
        }

        // set default subtitle
        if let Some(default_subtitle) = self.default_subtitle {
            if let Some(position) = subtitles
                .iter()
                .position(|m| m.language == default_subtitle)
            {
                if container_supports_softsubs {
                    match dst.extension().unwrap_or_default().to_str().unwrap() {
                        "mov" | "mp4" => output_presets.extend([
                            "-movflags".to_string(),
                            "faststart".to_string(),
                            "-c:s".to_string(),
                            "mov_text".to_string(),
                        ]),
                        _ => (),
                    }
                } else {
                    // remove '-c:v copy' and '-c:a copy' from output presets as its causes issues with
                    // burning subs into the video
                    let mut last = String::new();
                    let mut remove_count = 0;
                    for (i, s) in output_presets.clone().iter().enumerate() {
                        if (last == "-c:v" || last == "-c:a") && s == "copy" {
                            // remove last
                            output_presets.remove(i - remove_count - 1);
                            remove_count += 1;
                            output_presets.remove(i - remove_count);
                            remove_count += 1;
                        }
                        last = s.clone();
                    }

                    output_presets.extend([
                        "-vf".to_string(),
                        format!(
                            "ass='{}'",
                            // ffmpeg doesn't removes all ':' and '\' from the filename when using
                            // the ass filter. well, on windows these characters are used in
                            // absolute paths, so they have to be correctly escaped here
                            if cfg!(windows) {
                                subtitles
                                    .get(position)
                                    .unwrap()
                                    .path
                                    .to_str()
                                    .unwrap()
                                    .replace('\\', "\\\\")
                                    .replace(':', "\\:")
                            } else {
                                subtitles
                                    .get(position)
                                    .unwrap()
                                    .path
                                    .to_string_lossy()
                                    .to_string()
                            }
                        ),
                    ])
                }
            }

            if container_supports_softsubs {
                if let Some(position) = subtitles
                    .iter()
                    .position(|meta| meta.language == default_subtitle)
                {
                    command_args.extend([
                        format!("-disposition:s:s:{}", position),
                        "default".to_string(),
                    ])
                }
            }
        }

        // set the 'forced' flag to CC subtitles
        for (i, subtitle) in subtitles.iter().enumerate() {
            // well, checking if the title contains '(CC)' might not be the best solutions from a
            // performance perspective but easier than adjusting the `FFmpegMeta` struct
            if !subtitle.title.contains("(CC)") {
                continue;
            }

            command_args.extend([format!("-disposition:s:s:{}", i), "forced".to_string()])
        }

        // manually specifying the color model for the output file. this must be done manually
        // because some Crunchyroll episodes are encoded in a way that ffmpeg cannot re-encode
        command_args.extend(["-pix_fmt".to_string(), "yuv420p".to_string()]);

        command_args.extend(output_presets);
        if let Some(output_format) = self.output_format {
            command_args.extend(["-f".to_string(), output_format]);
        }

        // prepend './' to the path on linux since ffmpeg may interpret the path incorrectly if it's just the filename.
        // see https://github.com/crunchy-labs/crunchy-cli/issues/303 for example
        if !cfg!(windows)
            && dst
                .parent()
                .map_or(true, |p| p.to_string_lossy().is_empty())
        {
            command_args.push(Path::new("./").join(dst).to_string_lossy().to_string());
        } else {
            command_args.push(dst.to_string_lossy().to_string())
        }

        debug!("ffmpeg {}", command_args.join(" "));

        // create parent directory if it does not exist
        if let Some(parent) = dst.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?
            }
        }

        let ffmpeg = Command::new("ffmpeg")
            // pass ffmpeg stdout to real stdout only if output file is stdout
            .stdout(if dst.to_str().unwrap() == "-" {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped())
            .args(command_args)
            .spawn()?;
        let ffmpeg_progress_cancel = CancellationToken::new();
        let ffmpeg_progress_cancellation_token = ffmpeg_progress_cancel.clone();
        let ffmpeg_progress = tokio::spawn(async move {
            ffmpeg_progress(
                max_frames as u64,
                fifo,
                format!("{:<1$}", "Generating output file", fmt_space + 1),
                ffmpeg_progress_cancellation_token,
            )
            .await
        });

        let result = ffmpeg.wait_with_output()?;
        if !result.status.success() {
            ffmpeg_progress.abort();
            bail!("{}", String::from_utf8_lossy(result.stderr.as_slice()))
        }
        ffmpeg_progress_cancel.cancel();
        ffmpeg_progress.await?
    }

    async fn check_free_space(
        &self,
        dst: &Path,
    ) -> Result<(Option<(PathBuf, u64)>, Option<(PathBuf, u64)>)> {
        let mut all_variant_data = vec![];
        for format in &self.formats {
            all_variant_data.push(&format.video.0);
            all_variant_data.extend(format.audios.iter().map(|(a, _)| a))
        }
        let mut estimated_required_space: u64 = 0;
        for variant_data in all_variant_data {
            // nearly no overhead should be generated with this call(s) as we're using dash as
            // stream provider and generating the dash segments does not need any fetching of
            // additional (http) resources as hls segments would
            let segments = variant_data.segments().await?;

            // sum the length of all streams up
            estimated_required_space += estimate_variant_file_size(variant_data, &segments);
        }

        let tmp_stat = fs2::statvfs(temp_directory()).unwrap();
        let mut dst_file = if dst.is_absolute() {
            dst.to_path_buf()
        } else {
            env::current_dir()?.join(dst)
        };
        for ancestor in dst_file.ancestors() {
            if ancestor.exists() {
                dst_file = ancestor.to_path_buf();
                break;
            }
        }
        let dst_stat = fs2::statvfs(&dst_file).unwrap();

        let mut tmp_space = tmp_stat.available_space();
        let mut dst_space = dst_stat.available_space();

        // this checks if the partition the two directories are located on are the same to prevent
        // that the space fits both file sizes each but not together. this is done by checking the
        // total space if each partition and the free space of each partition (the free space can
        // differ by 10MB as some tiny I/O operations could be performed between the two calls which
        // are checking the disk space)
        if tmp_stat.total_space() == dst_stat.total_space()
            && (tmp_stat.available_space() as i64 - dst_stat.available_space() as i64).abs() < 10240
        {
            tmp_space *= 2;
            dst_space *= 2;
        }

        let mut tmp_required = None;
        let mut dst_required = None;

        if tmp_space < estimated_required_space {
            tmp_required = Some((temp_directory(), estimated_required_space))
        }
        if (!is_special_file(dst) && dst.to_string_lossy() != "-")
            && dst_space < estimated_required_space
        {
            dst_required = Some((dst_file, estimated_required_space))
        }
        Ok((tmp_required, dst_required))
    }

    async fn download_video(
        &self,
        variant_data: &VariantData,
        message: String,
    ) -> Result<TempPath> {
        let tempfile = tempfile(".mp4")?;
        let (mut file, path) = tempfile.into_parts();

        self.download_segments(&mut file, message, variant_data)
            .await?;

        Ok(path)
    }

    async fn download_audio(
        &self,
        variant_data: &VariantData,
        message: String,
    ) -> Result<TempPath> {
        let tempfile = tempfile(".m4a")?;
        let (mut file, path) = tempfile.into_parts();

        self.download_segments(&mut file, message, variant_data)
            .await?;

        Ok(path)
    }

    async fn download_subtitle(
        &self,
        subtitle: Subtitle,
        max_length: NaiveTime,
    ) -> Result<TempPath> {
        let tempfile = tempfile(".ass")?;
        let (mut file, path) = tempfile.into_parts();

        let mut buf = vec![];
        subtitle.write_to(&mut buf).await?;
        fix_subtitles(&mut buf, max_length);

        file.write_all(buf.as_slice())?;

        Ok(path)
    }

    async fn download_font(&self, name: &str) -> Result<Option<(PathBuf, bool)>> {
        let Some((_, font_file)) = FONTS.iter().find(|(f, _)| f == &name) else {
            return Ok(None);
        };

        let cache_dir = cache_dir("fonts")?;
        let file = cache_dir.join(font_file);
        if file.exists() {
            return Ok(Some((file, true)));
        }

        // the speed limiter does not apply to this
        let font = self
            .client
            .get(format!(
                "https://static.crunchyroll.com/vilos-v2/web/vilos/assets/libass-fonts/{}",
                font_file
            ))
            .send()
            .await?
            .bytes()
            .await?;
        fs::write(&file, font)?;

        Ok(Some((file, false)))
    }

    async fn download_segments(
        &self,
        writer: &mut impl Write,
        message: String,
        variant_data: &VariantData,
    ) -> Result<()> {
        let segments = variant_data.segments().await?;
        let total_segments = segments.len();

        let count = Arc::new(Mutex::new(0));

        let progress = if log::max_level() == LevelFilter::Info {
            let estimated_file_size = estimate_variant_file_size(variant_data, &segments);

            let progress = ProgressBar::new(estimated_file_size)
                .with_style(
                    ProgressStyle::with_template(
                        ":: {msg} {bytes:>10} {bytes_per_sec:>12} [{wide_bar}] {percent:>3}%",
                    )
                    .unwrap()
                    .progress_chars("##-"),
                )
                .with_message(message)
                .with_finish(ProgressFinish::Abandon);
            Some(progress)
        } else {
            None
        };

        let cpus = self.download_threads;
        let mut segs: Vec<Vec<VariantSegment>> = Vec::with_capacity(cpus);
        for _ in 0..cpus {
            segs.push(vec![])
        }
        for (i, segment) in segments.clone().into_iter().enumerate() {
            segs[i - ((i / cpus) * cpus)].push(segment);
        }

        let (sender, mut receiver) = unbounded_channel();

        let mut join_set: JoinSet<Result<()>> = JoinSet::new();
        for num in 0..cpus {
            let thread_sender = sender.clone();
            let thread_segments = segs.remove(0);
            let thread_client = self.client.clone();
            let mut thread_rate_limiter = self.rate_limiter.clone();
            let thread_count = count.clone();
            join_set.spawn(async move {
                let after_download_sender = thread_sender.clone();

                // the download process is encapsulated in its own function. this is done to easily
                // catch errors which get returned with `...?` and `bail!(...)` and that the thread
                // itself can report that an error has occurred
                let download = || async move {
                    for (i, segment) in thread_segments.into_iter().enumerate() {
                        let mut retry_count = 0;
                        let mut buf = loop {
                            let request = thread_client
                                .get(&segment.url)
                                .timeout(Duration::from_secs(60));
                            let response = if let Some(rate_limiter) = &mut thread_rate_limiter {
                                rate_limiter.call(request.build()?).await.map_err(anyhow::Error::new)
                            } else {
                                request.send().await.map_err(anyhow::Error::new)
                            };

                            let err = match response {
                                Ok(r) => match r.bytes().await {
                                    Ok(b) => break b.to_vec(),
                                    Err(e) => anyhow::Error::new(e)
                                }
                                Err(e) => e,
                            };

                            if retry_count == 5 {
                                bail!("Max retry count reached ({}), multiple errors occurred while receiving segment {}: {}", retry_count, num + (i * cpus), err)
                            }
                            debug!("Failed to download segment {} ({}). Retrying, {} out of 5 retries left", num + (i * cpus), err, 5 - retry_count);

                            retry_count += 1;
                        };

                        buf = VariantSegment::decrypt(&mut buf, segment.key)?.to_vec();

                        let mut c = thread_count.lock().await;
                        debug!(
                            "Downloaded and decrypted segment [{}/{} {:.2}%] {}",
                            num + (i * cpus) + 1,
                            total_segments,
                            ((*c + 1) as f64 / total_segments as f64) * 100f64,
                            segment.url
                        );

                        thread_sender.send((num as i32 + (i * cpus) as i32, buf))?;

                        *c += 1;
                    }
                    Ok(())
                };


                let result = download().await;
                if result.is_err() {
                    after_download_sender.send((-1, vec![]))?;
                }

                result
            });
        }
        // drop the sender already here so it does not outlive all download threads which are the only
        // real consumers of it
        drop(sender);

        // this is the main loop which writes the data. it uses a BTreeMap as a buffer as the write
        // happens synchronized. the download consist of multiple segments. the map keys are representing
        // the segment number and the values the corresponding bytes
        let mut data_pos = 0;
        let mut buf: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
        while let Some((pos, bytes)) = receiver.recv().await {
            // if the position is lower than 0, an error occurred in the sending download thread
            if pos < 0 {
                break;
            }

            if let Some(p) = &progress {
                let progress_len = p.length().unwrap();
                let estimated_segment_len = (variant_data.bandwidth / 8)
                    * segments.get(pos as usize).unwrap().length.as_secs();
                let bytes_len = bytes.len() as u64;

                p.set_length(progress_len - estimated_segment_len + bytes_len);
                p.inc(bytes_len)
            }

            // check if the currently sent bytes are the next in the buffer. if so, write them directly
            // to the target without first adding them to the buffer.
            // if not, add them to the buffer
            if data_pos == pos {
                writer.write_all(bytes.borrow())?;
                data_pos += 1;
            } else {
                buf.insert(pos, bytes);
            }
            // check if the buffer contains the next segment(s)
            while let Some(b) = buf.remove(&data_pos) {
                writer.write_all(b.borrow())?;
                data_pos += 1;
            }
        }

        // if any error has occurred while downloading it gets returned here
        while let Some(joined) = join_set.join_next().await {
            joined??
        }

        // write the remaining buffer, if existent
        while let Some(b) = buf.remove(&data_pos) {
            writer.write_all(b.borrow())?;
            data_pos += 1;
        }

        if !buf.is_empty() {
            bail!(
                "Download buffer is not empty. Remaining segments: {}",
                buf.into_keys()
                    .map(|k| k.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            )
        }

        Ok(())
    }
}

fn estimate_variant_file_size(variant_data: &VariantData, segments: &[VariantSegment]) -> u64 {
    (variant_data.bandwidth / 8) * segments.iter().map(|s| s.length.as_secs()).sum::<u64>()
}

/// Get the length and fps of a video.
fn get_video_stats(path: &Path) -> Result<(NaiveTime, f64)> {
    let video_length = Regex::new(r"Duration:\s(?P<time>\d+:\d+:\d+\.\d+),")?;
    let video_fps = Regex::new(r"(?P<fps>[\d/.]+)\sfps")?;

    let ffmpeg = Command::new("ffmpeg")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-y")
        .arg("-hide_banner")
        .args(["-i", path.to_str().unwrap()])
        .output()?;
    let ffmpeg_output = String::from_utf8(ffmpeg.stderr)?;
    let length_caps = video_length
        .captures(ffmpeg_output.as_str())
        .ok_or(anyhow::anyhow!(
            "failed to get video length: {}",
            ffmpeg_output
        ))?;
    let fps_caps = video_fps
        .captures(ffmpeg_output.as_str())
        .ok_or(anyhow::anyhow!(
            "failed to get video fps: {}",
            ffmpeg_output
        ))?;

    Ok((
        NaiveTime::parse_from_str(length_caps.name("time").unwrap().as_str(), "%H:%M:%S%.f")
            .unwrap(),
        fps_caps.name("fps").unwrap().as_str().parse().unwrap(),
    ))
}

// all subtitle fonts (extracted from javascript)
const FONTS: [(&str, &str); 68] = [
    ("Adobe Arabic", "AdobeArabic-Bold.woff2"),
    ("Andale Mono", "andalemo.woff2"),
    ("Arial", "arial.woff2"),
    ("Arial Black", "ariblk.woff2"),
    ("Arial Bold", "arialbd.woff2"),
    ("Arial Bold Italic", "arialbi.woff2"),
    ("Arial Italic", "ariali.woff2"),
    ("Arial Unicode MS", "arialuni.woff2"),
    ("Comic Sans MS", "comic.woff2"),
    ("Comic Sans MS Bold", "comicbd.woff2"),
    ("Courier New", "cour.woff2"),
    ("Courier New Bold", "courbd.woff2"),
    ("Courier New Bold Italic", "courbi.woff2"),
    ("Courier New Italic", "couri.woff2"),
    ("DejaVu LGC Sans Mono", "DejaVuLGCSansMono.woff2"),
    ("DejaVu LGC Sans Mono Bold", "DejaVuLGCSansMono-Bold.woff2"),
    (
        "DejaVu LGC Sans Mono Bold Oblique",
        "DejaVuLGCSansMono-BoldOblique.woff2",
    ),
    (
        "DejaVu LGC Sans Mono Oblique",
        "DejaVuLGCSansMono-Oblique.woff2",
    ),
    ("DejaVu Sans", "DejaVuSans.woff2"),
    ("DejaVu Sans Bold", "DejaVuSans-Bold.woff2"),
    ("DejaVu Sans Bold Oblique", "DejaVuSans-BoldOblique.woff2"),
    ("DejaVu Sans Condensed", "DejaVuSansCondensed.woff2"),
    (
        "DejaVu Sans Condensed Bold",
        "DejaVuSansCondensed-Bold.woff2",
    ),
    (
        "DejaVu Sans Condensed Bold Oblique",
        "DejaVuSansCondensed-BoldOblique.woff2",
    ),
    (
        "DejaVu Sans Condensed Oblique",
        "DejaVuSansCondensed-Oblique.woff2",
    ),
    ("DejaVu Sans ExtraLight", "DejaVuSans-ExtraLight.woff2"),
    ("DejaVu Sans Mono", "DejaVuSansMono.woff2"),
    ("DejaVu Sans Mono Bold", "DejaVuSansMono-Bold.woff2"),
    (
        "DejaVu Sans Mono Bold Oblique",
        "DejaVuSansMono-BoldOblique.woff2",
    ),
    ("DejaVu Sans Mono Oblique", "DejaVuSansMono-Oblique.woff2"),
    ("DejaVu Sans Oblique", "DejaVuSans-Oblique.woff2"),
    ("Gautami", "gautami.woff2"),
    ("Georgia", "georgia.woff2"),
    ("Georgia Bold", "georgiab.woff2"),
    ("Georgia Bold Italic", "georgiaz.woff2"),
    ("Georgia Italic", "georgiai.woff2"),
    ("Impact", "impact.woff2"),
    ("Mangal", "MANGAL.woff2"),
    ("Meera Inimai", "MeeraInimai-Regular.woff2"),
    ("Noto Sans Tamil", "NotoSansTamil.woff2"),
    ("Noto Sans Telugu", "NotoSansTelegu.woff2"),
    ("Noto Sans Thai", "NotoSansThai.woff2"),
    ("Rubik", "Rubik-Regular.woff2"),
    ("Rubik Black", "Rubik-Black.woff2"),
    ("Rubik Black Italic", "Rubik-BlackItalic.woff2"),
    ("Rubik Bold", "Rubik-Bold.woff2"),
    ("Rubik Bold Italic", "Rubik-BoldItalic.woff2"),
    ("Rubik Italic", "Rubik-Italic.woff2"),
    ("Rubik Light", "Rubik-Light.woff2"),
    ("Rubik Light Italic", "Rubik-LightItalic.woff2"),
    ("Rubik Medium", "Rubik-Medium.woff2"),
    ("Rubik Medium Italic", "Rubik-MediumItalic.woff2"),
    ("Tahoma", "tahoma.woff2"),
    ("Times New Roman", "times.woff2"),
    ("Times New Roman Bold", "timesbd.woff2"),
    ("Times New Roman Bold Italic", "timesbi.woff2"),
    ("Times New Roman Italic", "timesi.woff2"),
    ("Trebuchet MS", "trebuc.woff2"),
    ("Trebuchet MS Bold", "trebucbd.woff2"),
    ("Trebuchet MS Bold Italic", "trebucbi.woff2"),
    ("Trebuchet MS Italic", "trebucit.woff2"),
    ("Verdana", "verdana.woff2"),
    ("Verdana Bold", "verdanab.woff2"),
    ("Verdana Bold Italic", "verdanaz.woff2"),
    ("Verdana Italic", "verdanai.woff2"),
    ("Vrinda", "vrinda.woff2"),
    ("Vrinda Bold", "vrindab.woff2"),
    ("Webdings", "webdings.woff2"),
];
lazy_static::lazy_static! {
    static ref FONT_REGEX: Regex = Regex::new(r"(?m)^Style:\s.+?,(?P<font>.+?),").unwrap();
}

/// Get the fonts used in the subtitle.
fn get_subtitle_stats(path: &Path) -> Result<Vec<String>> {
    let mut fonts = vec![];

    for capture in FONT_REGEX.captures_iter(&(fs::read_to_string(path)?)) {
        if let Some(font) = capture.name("font") {
            let font_string = font.as_str().to_string();
            if !fonts.contains(&font_string) {
                fonts.push(font_string)
            }
        }
    }

    Ok(fonts)
}

/// Fix the subtitles in multiple ways as Crunchyroll sometimes delivers them malformed.
///
/// Look and feel fix: Add `ScaledBorderAndShadows: yes` to subtitles; without it they look very
/// messy on some video players. See
/// [crunchy-labs/crunchy-cli#66](https://github.com/crunchy-labs/crunchy-cli/issues/66) for more
/// information.
/// Length fix: Sometimes subtitles have an unnecessary long entry which exceeds the video length,
/// some video players can't handle this correctly. To prevent this, the subtitles must be checked
/// if any entry is longer than the video length and if so the entry ending must be hard set to not
/// exceed the video length. See [crunchy-labs/crunchy-cli#32](https://github.com/crunchy-labs/crunchy-cli/issues/32)
/// for more information.
/// Sort fix: Sometimes subtitle entries aren't sorted correctly by time which confuses some video
/// players. To prevent this, the subtitle entries must be manually sorted. See
/// [crunchy-labs/crunchy-cli#208](https://github.com/crunchy-labs/crunchy-cli/issues/208) for more
/// information.
fn fix_subtitles(raw: &mut Vec<u8>, max_length: NaiveTime) {
    let re = Regex::new(
        r"^Dialogue:\s(?P<layer>\d+),(?P<start>\d+:\d+:\d+\.\d+),(?P<end>\d+:\d+:\d+\.\d+),",
    )
    .unwrap();

    // chrono panics if we try to format NaiveTime with `%2f` and the nano seconds has more than 2
    // digits so them have to be reduced manually to avoid the panic
    fn format_naive_time(native_time: NaiveTime) -> String {
        let formatted_time = native_time.format("%f").to_string();
        format!(
            "{}.{}",
            native_time.format("%T"),
            if formatted_time.len() <= 2 {
                native_time.format("%2f").to_string()
            } else {
                formatted_time.split_at(2).0.parse().unwrap()
            }
        )
        .split_off(1) // <- in the ASS spec, the hour has only one digit
    }

    let mut entries = (vec![], vec![]);

    let mut as_lines: Vec<String> = String::from_utf8_lossy(raw.as_slice())
        .split('\n')
        .map(|s| s.to_string())
        .collect();

    for (i, line) in as_lines.iter_mut().enumerate() {
        if line.trim() == "[Script Info]" {
            line.push_str("\nScaledBorderAndShadow: yes")
        } else if let Some(capture) = re.captures(line) {
            let mut start = capture.name("start").map_or(NaiveTime::default(), |s| {
                NaiveTime::parse_from_str(s.as_str(), "%H:%M:%S.%f").unwrap()
            });
            let mut end = capture.name("end").map_or(NaiveTime::default(), |e| {
                NaiveTime::parse_from_str(e.as_str(), "%H:%M:%S.%f").unwrap()
            });

            if start > max_length || end > max_length {
                let layer = capture
                    .name("layer")
                    .map_or(0, |l| i32::from_str(l.as_str()).unwrap());

                if start > max_length {
                    start = max_length;
                }
                if start > max_length || end > max_length {
                    end = max_length;
                }

                *line = re
                    .replace(
                        line,
                        format!(
                            "Dialogue: {},{},{},",
                            layer,
                            format_naive_time(start),
                            format_naive_time(end)
                        ),
                    )
                    .to_string()
            }
            entries.0.push((start, i));
            entries.1.push(i)
        }
    }

    entries.0.sort_by(|(a, _), (b, _)| a.cmp(b));
    for i in 0..entries.0.len() {
        let (_, original_position) = entries.0[i];
        let new_position = entries.1[i];

        if original_position != new_position {
            as_lines.swap(original_position, new_position)
        }
    }

    *raw = as_lines.join("\n").into_bytes()
}

fn write_ffmpeg_chapters(
    file: &mut fs::File,
    video_len: NaiveTime,
    events: &mut Vec<(&str, &SkipEventsEvent)>,
) -> Result<()> {
    let video_len = video_len
        .signed_duration_since(NaiveTime::MIN)
        .num_seconds() as u32;
    events.sort_by(|(_, event_a), (_, event_b)| event_a.start.cmp(&event_b.start));

    writeln!(file, ";FFMETADATA1")?;

    let mut last_end_time = 0;
    for (name, event) in events {
        // include an extra 'Episode' chapter if the start of the current chapter is more than 10
        // seconds later than the end of the last chapter.
        // this is done before writing the actual chapter of this loop to keep the chapter
        // chronologically in order
        if event.start as i32 - last_end_time as i32 > 10 {
            writeln!(file, "[CHAPTER]")?;
            writeln!(file, "TIMEBASE=1/1")?;
            writeln!(file, "START={}", last_end_time)?;
            writeln!(file, "END={}", event.start)?;
            writeln!(file, "title=Episode")?;
        }

        writeln!(file, "[CHAPTER]")?;
        writeln!(file, "TIMEBASE=1/1")?;
        writeln!(file, "START={}", event.start)?;
        writeln!(file, "END={}", event.end)?;
        writeln!(file, "title={}", name)?;

        last_end_time = event.end;
    }

    // only add a traling chapter if the gab between the end of the last chapter and the total video
    // length is greater than 10 seconds
    if video_len as i32 - last_end_time as i32 > 10 {
        writeln!(file, "[CHAPTER]")?;
        writeln!(file, "TIMEBASE=1/1")?;
        writeln!(file, "START={}", last_end_time)?;
        writeln!(file, "END={}", video_len)?;
        writeln!(file, "title=Episode")?;
    }

    Ok(())
}

async fn ffmpeg_progress<R: AsyncReadExt + Unpin>(
    total_frames: u64,
    stats: R,
    message: String,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let current_frame = Regex::new(r"frame=\s+(?P<frame>\d+)")?;

    let progress = if log::max_level() == LevelFilter::Info {
        let progress = ProgressBar::new(total_frames)
            .with_style(
                ProgressStyle::with_template(":: {msg} [{wide_bar}] {percent:>3}%")
                    .unwrap()
                    .progress_chars("##-"),
            )
            .with_message(message)
            .with_finish(ProgressFinish::Abandon);
        progress.set_draw_target(ProgressDrawTarget::stdout());
        progress.enable_steady_tick(Duration::from_millis(200));
        Some(progress)
    } else {
        None
    };

    let reader = BufReader::new(stats);
    let mut lines = reader.lines();
    loop {
        select! {
            // when gracefully canceling this future, set the progress to 100% (finished). sometimes
            // ffmpeg is too fast or already finished when the reading process of 'stats' starts
            // which causes the progress to be stuck at 0%
            _ = cancellation_token.cancelled() => {
                if let Some(p) = &progress {
                    p.set_position(total_frames)
                }
                debug!(
                    "Processed frame [{}/{} 100%]",
                    total_frames,
                    total_frames
                );
                return Ok(())
            }
            line = lines.next_line() => {
                let Some(line) = line? else {
                    break
                };

                // we're manually unpack the regex here as `.unwrap()` may fail in some cases, e.g.
                // https://github.com/crunchy-labs/crunchy-cli/issues/337
                let Some(frame_cap) = current_frame.captures(line.as_str()) else {
                    break
                };
                let Some(frame_str) = frame_cap.name("frame") else {
                    break
                };
                let frame: u64 = frame_str.as_str().parse()?;

                if let Some(p) = &progress {
                    p.set_position(frame)
                }

                debug!(
                    "Processed frame [{}/{} {:.2}%]",
                    frame,
                    total_frames,
                    (frame as f64 / total_frames as f64) * 100f64
                )
            }
        }
    }

    Ok(())
}
