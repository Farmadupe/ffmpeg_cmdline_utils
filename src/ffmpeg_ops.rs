use std::{
    ffi::OsStr,
    io::prelude::*,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, SystemTime},
};

#[cfg(target_family = "windows")]
use std::os::windows::process::CommandExt;

use FfmpegCommandName::*;
use FfmpegErrorKind::*;

use crate::*;

const FFPROBE_TIMEOUT_SECS: usize = 60;

#[derive(Debug)]
pub struct FfmpegFrames {
    x: u32,
    y: u32,
    child: std::process::Child,
    num_frames: u32,
    frames_read: u32,
    timeout_time: SystemTime,
    finished: bool,
}

impl Iterator for FfmpegFrames {
    type Item = image::RgbImage;

    fn next(&mut self) -> Option<Self::Item> {
        //Check exit conditions
        let read_enough_frames = self.frames_read >= self.num_frames;
        let exceeded_timeout = SystemTime::now() > self.timeout_time;

        if self.finished || read_enough_frames || exceeded_timeout {
            self.finished = true;
            let _kill_error = self.child.kill();
            let _wait_error = self.child.wait();
            return None;
        }

        let mut raw_buf = vec![0u8; (self.x * self.y * 3) as usize];

        // Otherwise wait for the frame until the timeout is exceeded
        let stdout = self.child.stdout.as_mut().unwrap();
        let mut buf_head = 0;
        while buf_head < raw_buf.len() {
            //abort on timeout.
            if SystemTime::now() > self.timeout_time {
                self.finished = true;
                return None;
            }

            let slice_to_read_into = &mut raw_buf[buf_head..];
            match stdout.read(slice_to_read_into) {
                //something went wrong, or no more data can be read
                Err(_) | Ok(0) => {
                    self.finished = true;
                    return None;
                }

                Ok(bytes_read) => buf_head += bytes_read,
            }

            //sleep for a small amount of time;
            std::thread::sleep(Duration::from_millis(10));
        }

        self.frames_read += 1;
        Some(image::RgbImage::from_raw(self.x, self.y, raw_buf).unwrap())
    }
}

// to prevent accumulation of zombie processes, reap the return code of
// ffmpeg subcommands (if nothing else has done so already) here
impl Drop for FfmpegFrames {
    fn drop(&mut self) {
        let _kill_error = self.child.kill();
        let _wait_error = self.child.wait();
    }
}

pub struct FfmpegFrameReaderBuilder {
    src_path: PathBuf,
    fps: Option<String>,
    num_frames: Option<u32>,
    timeout_secs: Option<u64>,
}

impl FfmpegFrameReaderBuilder {
    pub fn new(src_path: PathBuf) -> Self {
        Self {
            src_path,
            fps: None,
            num_frames: None,
            timeout_secs: None,
        }
    }

    pub fn fps(&mut self, fps: impl AsRef<str>) -> &mut Self {
        self.fps = Some(fps.as_ref().to_string());
        self
    }

    pub fn num_frames(&mut self, num_frames: u32) -> &mut Self {
        self.num_frames = Some(num_frames);
        self
    }

    pub fn timeout_secs(&mut self, timeout_secs: u64) -> &mut Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    pub fn spawn(&self) -> Result<(FfmpegFrames, VideoInfo), FfmpegErrorKind> {
        //we also need to find out the resolution of the video so that stdout can be converted into frames.
        let stats =
            VideoInfo::new(&self.src_path).map_err(|e| FfmpegErrorKind::Io(e.to_string()))?;

        //bail out if we get invalid dimensions.
        let (x, y) = stats.resolution();
        if x == 0 || y == 0 {
            return Err(FfmpegErrorKind::InvalidResolution);
        }

        let fps_string: String;
        let fps_arg = match self.fps {
            Some(ref fps) => {
                fps_string = format!("fps={}", fps);
                vec![OsStr::new("-vf"), OsStr::new(&fps_string)]
            }
            None => vec![],
        };

        let num_frames_string: String;
        let num_frames_arg = match self.num_frames {
            Some(ref num_frames) => {
                num_frames_string = num_frames.to_string();
                vec![OsStr::new("-vframes"), OsStr::new(&num_frames_string)]
            }
            None => vec![],
        };

        #[rustfmt::skip]
        let mut args = vec![
            OsStr::new("-hide_banner"),
            OsStr::new("-loglevel"), OsStr::new("warning"),
            OsStr::new("-nostats"),
            // OsStr::new("-ss"),       OsStr::new("00:00:30"),        
            OsStr::new("-i"),        OsStr::new(&self.src_path),
        ];

        args.extend(fps_arg);
        args.extend(num_frames_arg);

        #[rustfmt::skip]
        args.extend(&[
            OsStr::new("-pix_fmt"),  OsStr::new("rgb24"),
            OsStr::new("-c:v"),      OsStr::new("rawvideo"),
            OsStr::new("-f"),        OsStr::new("image2pipe"),
            OsStr::new("-")
        ]);

        //println!("{:?}", args);

        let mut child = spawn_ffmpeg_command(Ffmpeg, &args, true)?;

        //Prevent possible lockup if stderr gets full by dropping the
        //handle from our side
        std::mem::drop(child.stderr.take());

        let (x, y) = stats.resolution;

        let frame_iterator = FfmpegFrames {
            x,
            y,
            child,
            num_frames: self.num_frames.unwrap_or(u32::MAX),
            frames_read: 0,
            timeout_time: SystemTime::now()
                + Duration::from_secs(self.timeout_secs.unwrap_or(u32::MAX as u64)), // (just in case u64::MAX has wraparound issues)
            finished: false,
        };

        //Ok((frames, stats))
        Ok((frame_iterator, stats))
    }
}

pub fn get_video_stats<P: AsRef<Path>>(src_path: P) -> Result<String, FfmpegErrorKind> {
    let args = &[
        OsStr::new("-v"),
        OsStr::new("quiet"),
        OsStr::new("-show_format"),
        OsStr::new("-show_streams"),
        OsStr::new("-print_format"),
        OsStr::new("json"),
        OsStr::new(src_path.as_ref()),
    ];

    let stdout = run_ffmpeg_command(Ffprobe, args, true)?.stdout;

    String::from_utf8(stdout).map_err(|_| Utf8Conversion)
}

pub fn is_video_file<P: AsRef<Path>>(src_path: P) -> Result<bool, FfmpegErrorKind> {
    fn get_ffprobe_output<P: AsRef<Path>>(src_path: P) -> Result<String, FfmpegErrorKind> {
        //"ffprobe -v error -select_streams v -show_entries stream=codec_type,codec_name,duration -of compact=p=0:nk=1 {}"

        #[rustfmt::skip]
        let args = &[
            OsStr::new("-v"),              OsStr::new("error"),
            OsStr::new("-select_streams"), OsStr::new("v"),
            OsStr::new("-show_entries"),   OsStr::new("stream=codec_type,codec_name,duration"),
            OsStr::new("-of"),             OsStr::new("compact=p=0:nk=1"),
            OsStr::new(src_path.as_ref())
        ];

        run_ffmpeg_command(Ffprobe, args, true).and_then(|output| {
            String::from_utf8(output.stdout)
                .map_err(|_| Utf8Conversion)
                .map(|s| s.trim().to_string())
        })
    }

    let streams_string = get_ffprobe_output(src_path.as_ref())?;

    let mut fields_iter = streams_string.split('|');

    let _codec_name = fields_iter.next().unwrap_or("");
    let codec_type = fields_iter.next().unwrap_or("");
    let duration = fields_iter
        .next()
        .unwrap_or("")
        .trim()
        .parse::<f64>()
        .unwrap_or(999.0);

    if codec_type != "video" {
        return Ok(false);
    }

    if duration < 1.0 {
        return Ok(false);
    }

    Ok(true)
}

pub fn ffmpeg_and_ffprobe_are_callable() -> bool {
    //check ffprobe is callable.
    if run_ffmpeg_command(Ffprobe, &[OsStr::new("-version")], true).is_err() {
        return false;
    }

    //now ffmpeg.
    if run_ffmpeg_command(Ffmpeg, &[OsStr::new("-version")], true).is_err() {
        return false;
    }

    true
}

#[derive(Debug, Clone, Copy)]
enum FfmpegCommandName {
    Ffprobe,
    Ffmpeg,
}

impl FfmpegCommandName {
    pub fn as_os_str(&self) -> &'static OsStr {
        match self {
            Self::Ffprobe => OsStr::new("ffprobe"),
            Self::Ffmpeg => OsStr::new("ffmpeg"),
        }
    }
}

fn spawn_ffmpeg_command(
    name: FfmpegCommandName,
    args: &[&OsStr],
    stderr_null: bool,
) -> Result<Child, FfmpegErrorKind> {
    use FfmpegErrorKind::*;

    let stderr_cfg = if stderr_null {
        Stdio::null()
    } else {
        Stdio::piped()
    };

    let mut command = Command::new(name.as_os_str());
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_cfg);

    //do not spawn a command window on windows when when in a gui application
    #[cfg(target_family = "windows")]
    command.creation_flags(winapi::um::winbase::CREATE_NO_WINDOW);

    command.spawn().map_err(|e| match e.kind() {
        //shell failed to execute the command. Separate out FileNotFound from all other errors
        //as by far the most likely cause is ffmpeg is not installed.
        std::io::ErrorKind::NotFound => FfmpegNotFound,
        _ => Io(format!("{:?}", e.kind())),
    })
}

struct FfmpegOutput {
    _stderr: Vec<u8>,
    stdout: Vec<u8>,
}

type FfmpegCmdResult = Result<FfmpegOutput, FfmpegErrorKind>;

fn run_ffmpeg_command(
    name: FfmpegCommandName,
    args: &[&OsStr],
    stderr_null: bool,
) -> FfmpegCmdResult {
    fn truncate_ffmpeg_err_msg(stderr: Vec<u8>) -> FfmpegErrorKind {
        match std::str::from_utf8(&stderr) {
            Ok(error_text) => FfmpegInternal(error_text.chars().take(500).collect::<String>()),
            Err(_) => Utf8Conversion,
        }
    }

    //Wait for the ffmpeg operation to complete FFMPEG_TIMEOUT_SECS
    let mut child = spawn_ffmpeg_command(name, args, stderr_null)?;

    //Accumulators for output
    let mut stdout = child.stdout.take().expect("Failed to obtain stdout");

    let mut stderr = (!stderr_null).then(|| child.stderr.take().expect("Failed to obtain stderr"));

    let mut timeout_counter_secs = 0;

    //We will assume that ffmpeg/ffprobe will usually complete in the first 1 sec. To keep this program responsive we will check for results at a rate of 100hz.
    //Then we will switch to checking at 1 Hz.
    let thread = std::thread::spawn(move || -> std::io::Result<ExitStatus> {
        let max_initial_fast_counts = 100;
        let mut initial_fast_counts = 0;
        let mut maybe_status;
        while timeout_counter_secs < FFPROBE_TIMEOUT_SECS {
            maybe_status = child.try_wait();
            match maybe_status {
                Err(e) => return Err(e),
                Ok(None) => {
                    if initial_fast_counts < max_initial_fast_counts {
                        std::thread::sleep(Duration::from_millis(10));
                        initial_fast_counts += 1;
                        if initial_fast_counts == max_initial_fast_counts {
                            timeout_counter_secs += 1;
                        }
                    } else {
                        std::thread::sleep(Duration::from_millis(1_000));
                        timeout_counter_secs += 1;
                    }
                }
                Ok(Some(s)) => return Ok(s),
            }
        }

        //timed out
        Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
    });

    //read from stdout and stderr
    let mut stdout_done = false;
    let mut stderr_done = stderr_null;

    //Buffer for stdout and stderr
    let mut read_buf = [0u8; 4096];
    let mut stdout_acc = vec![];
    let mut stderr_acc = vec![];

    while !(stdout_done && stderr_done) {
        if !stdout_done {
            match stdout.read(&mut read_buf) {
                Err(_) | Ok(0) => stdout_done = true,
                Ok(amount) => {
                    stdout_acc
                        .write_all(&read_buf[..amount])
                        .expect("failed to append to string");
                }
            }
        }

        if !stderr_done {
            match stderr.as_mut().unwrap().read(&mut read_buf) {
                Err(_) | Ok(0) => stderr_done = true,
                Ok(amount) => {
                    stderr_acc
                        .write_all(&read_buf[..amount])
                        .expect("failed to append to string");
                }
            }
        }
    }

    let exit_status = thread.join().expect("thread couldn't join");

    match exit_status {
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => Err(FfmpegNotFound),
            _ => Err(Io(format!("{:?}", e.kind()))),
        },
        //The shell successfully executed it, but maybe it returned an error code
        Ok(status) => {
            if status.success() {
                Ok(FfmpegOutput {
                    stdout: stdout_acc,
                    _stderr: stderr_acc,
                })
            } else {
                //sometimes ffmpeg creates very long error messages. Limit them to the first 500 characters
                Err(truncate_ffmpeg_err_msg(stderr_acc))
            }
        }
    }
}
