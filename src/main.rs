use clap::{Args, Parser, Subcommand, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::{imageops::FilterType, ImageBuffer, Rgb};
use nokhwa::{
    pixel_format::RgbFormat,
    query,
    utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType},
    Camera,
};
use std::{
    fmt::Write as _,
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

type RgbImg = ImageBuffer<Rgb<u8>, Vec<u8>>;

// 70 levels, light → dark
const RAMP: &[u8] =
    b" .'`^\",:;Il!i><~+_-?][}{1)(|\\/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@$";

#[derive(Parser)]
#[command(
    name = "ascii_shader",
    about = "Render images and live camera feeds as ASCII art",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert a still image to ASCII
    Image {
        /// Path to the image
        path: PathBuf,
        #[command(flatten)]
        opts: RenderOpts,
    },
    /// Stream a live camera feed as ASCII (q or Esc to quit)
    Camera {
        /// Camera index — run `list-cameras` to see available devices.
        /// On macOS, your iPhone via Continuity Camera shows up here too.
        #[arg(short, long, default_value_t = 0)]
        index: u32,
        /// Force selection of the iPhone (Continuity Camera), ignoring --index.
        /// Scans available devices and picks the first Continuity match.
        #[arg(long, short = 'c')]
        continuity: bool,
        #[command(flatten)]
        opts: RenderOpts,
    },
    /// List available camera devices
    ListCameras,
}

#[derive(Args, Clone)]
struct RenderOpts {
    /// Rendering style. `ascii` uses a brightness ramp of characters (default);
    /// `blocks` swaps in Unicode half-blocks for a 2× sharper, non-ASCII pixel look.
    #[arg(long, value_enum, default_value_t = Mode::Ascii)]
    mode: Mode,
    /// Output width in characters (defaults to terminal width)
    #[arg(short = 'w', long)]
    width: Option<u32>,
    /// Disable ANSI color (forces ASCII mode regardless of --mode)
    #[arg(long)]
    no_color: bool,
    /// Invert brightness
    #[arg(long)]
    invert: bool,
    /// Brightness multiplier; 1.0 = unchanged
    #[arg(long, default_value_t = 1.0)]
    brightness: f32,
    /// Mirror horizontally (handy for selfie cameras)
    #[arg(long)]
    mirror: bool,
    /// Stretch to fill the whole terminal, ignoring source aspect ratio
    #[arg(long)]
    fill: bool,
}

#[derive(Copy, Clone, ValueEnum, PartialEq, Eq)]
enum Mode {
    /// Classic ASCII brightness ramp.
    Ascii,
    /// Half-block (▀) renderer — 2 pixels per cell, full RGB. Sharpest.
    Blocks,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Image { path, opts } => run_image(&path, &opts),
        Command::Camera {
            index,
            continuity,
            opts,
        } => {
            let resolved = if continuity {
                pick_continuity_index()?
            } else {
                index
            };
            run_camera(resolved, &opts)
        }
        Command::ListCameras => list_cameras(),
    }
}

fn pick_continuity_index() -> Result<u32, Box<dyn std::error::Error>> {
    let cams = query(ApiBackend::Auto)?;
    for c in &cams {
        let desc = c.description().to_lowercase();
        let name = c.human_name().to_lowercase();
        // Match the iPhone by its AVFoundation device-type marker (or model name).
        // Exclude "Desk View" — that's a separate iPhone-backed camera type.
        let looks_continuity = desc.contains("avcapturedevicetypecontinuitycamera")
            || desc.contains("avcapturedevicetypeexternal")
            || desc.contains("iphone");
        let is_desk_view = name.contains("desk view") || desc.contains("desk view");
        if !looks_continuity || is_desk_view {
            continue;
        }
        if let CameraIndex::Index(i) = c.index() {
            eprintln!(
                "using Continuity Camera at index {i}: {}",
                c.human_name()
            );
            return Ok(*i);
        }
    }
    Err("No Continuity Camera found. Wake your iPhone, make sure both devices are signed \
         into the same Apple ID, and keep them nearby with Wi-Fi + Bluetooth on."
        .into())
}

fn list_cameras() -> Result<(), Box<dyn std::error::Error>> {
    let cams = query(ApiBackend::Auto)?;
    if cams.is_empty() {
        println!("No cameras found.");
        #[cfg(target_os = "macos")]
        println!(
            "Tip: for iPhone Continuity Camera, sign in to the same Apple ID on \
             both devices, keep them nearby with Wi-Fi + Bluetooth on, and unlock the iPhone."
        );
        return Ok(());
    }
    println!("{} camera(s) available:", cams.len());
    for c in cams {
        println!(
            "  [{}] {}  —  {}",
            c.index(),
            c.human_name(),
            c.description()
        );
    }
    Ok(())
}

fn run_image(path: &PathBuf, opts: &RenderOpts) -> Result<(), Box<dyn std::error::Error>> {
    let src = image::open(path)?.to_rgb8();
    let mode = effective_mode(opts);
    let (term_w, term_h) = terminal::size().unwrap_or((100, 40));
    let max_w = opts.width.unwrap_or(term_w as u32).max(10);
    let max_h = (term_h as u32).saturating_sub(2).max(2);
    let (w, h) = fit_image(src.width(), src.height(), max_w, max_h, mode, opts.fill);
    let img = image::imageops::resize(&src, w, h, FilterType::Lanczos3);
    let img = if opts.mirror {
        image::imageops::flip_horizontal(&img)
    } else {
        img
    };

    let mut buf = String::with_capacity((w as usize + 32) * h as usize);
    render_frame(&img, opts, mode, &mut buf);
    let mut out = BufWriter::new(io::stdout().lock());
    out.write_all(buf.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn run_camera(index: u32, opts: &RenderOpts) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        cursor::Hide,
        Clear(ClearType::All)
    )?;

    // q/Esc/Ctrl+C must work even if Camera::new / open_stream hangs (which
    // happens, e.g., picking the built-in FaceTime camera while the laptop is
    // in clamshell mode, or the Continuity index when the iPhone isn't awake).
    // We address that with two side threads:
    //   1. Input thread — listens for quit keys, force-exits 1s after one is pressed.
    //   2. Startup watchdog — if no frame has rendered within 5s, force-exits
    //      with a helpful message even when no keypress arrives.
    let quit = Arc::new(AtomicBool::new(false));
    let main_done = Arc::new(AtomicBool::new(false));
    let render_started = Arc::new(AtomicBool::new(false));
    let input_thread = spawn_input_thread(quit.clone(), main_done.clone());
    let _watchdog = spawn_startup_watchdog(render_started.clone(), main_done.clone(), index);

    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        let format = RequestedFormat::new::<RgbFormat>(
            RequestedFormatType::AbsoluteHighestFrameRate,
        );
        let mut camera = Camera::new(CameraIndex::Index(index), format)?;
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        camera.open_stream()?;
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        let r = camera_loop(&mut camera, opts, &quit, &render_started);
        let _ = camera.stop_stream();
        r
    })();

    main_done.store(true, Ordering::Relaxed);
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    let _ = input_thread.join();
    result
}

fn spawn_startup_watchdog(
    render_started: Arc<AtomicBool>,
    main_done: Arc<AtomicBool>,
    index: u32,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        // Sleep in 100ms ticks so we can bail early if main exits cleanly.
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(100));
            if render_started.load(Ordering::Relaxed)
                || main_done.load(Ordering::Relaxed)
            {
                return;
            }
        }
        let mut so = io::stdout();
        let _ = execute!(so, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        eprintln!(
            "camera index {index} didn't deliver a frame in 5s — likely unavailable."
        );
        #[cfg(target_os = "macos")]
        eprintln!(
            "(Mac in clamshell? FaceTime HD is off when the lid is closed.\n\
             Run `list-cameras` and try another --index, or wake the iPhone for Continuity.)"
        );
        std::process::exit(1);
    })
}

fn spawn_input_thread(
    quit: Arc<AtomicBool>,
    main_done: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        if main_done.load(Ordering::Relaxed) {
            return;
        }
        if !matches!(event::poll(Duration::from_millis(100)), Ok(true)) {
            continue;
        }
        let Ok(Event::Key(k)) = event::read() else { continue };
        let is_quit = matches!(
            k.code,
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
        ) || (matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C'))
            && k.modifiers.contains(KeyModifiers::CONTROL));
        if !is_quit {
            continue;
        }
        quit.store(true, Ordering::Relaxed);
        // Watchdog: if main hasn't acknowledged within 1s (camera wedged
        // inside Camera::new / open_stream / frame()), restore the terminal
        // and force-exit so the user is never stuck.
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(50));
            if main_done.load(Ordering::Relaxed) {
                return;
            }
        }
        let mut so = io::stdout();
        let _ = execute!(so, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        eprintln!("camera unresponsive — forced exit");
        std::process::exit(0);
    })
}

fn camera_loop(
    camera: &mut Camera,
    opts: &RenderOpts,
    quit: &Arc<AtomicBool>,
    render_started: &Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = BufWriter::with_capacity(1 << 20, io::stdout().lock());
    let mut buf = String::with_capacity(1 << 20);
    let mut last = Instant::now();
    let mut fps = 0.0_f32;

    while !quit.load(Ordering::Relaxed) {
        let raw = camera.frame()?;
        render_started.store(true, Ordering::Relaxed);
        let decoded: RgbImg = raw.decode_image::<RgbFormat>()?;
        let mode = effective_mode(opts);
        let (term_w, term_h) = terminal::size().unwrap_or((100, 40));
        // Match `image` mode at width 120 by default; explicit -w still wins.
        let max_w = opts
            .width
            .unwrap_or_else(|| 120u32.min(term_w as u32))
            .max(10);
        let max_h = (term_h as u32).saturating_sub(2).max(2);
        let (w, h) =
            fit_image(decoded.width(), decoded.height(), max_w, max_h, mode, opts.fill);
        let resized = image::imageops::resize(&decoded, w, h, FilterType::Triangle);
        let resized = if opts.mirror {
            image::imageops::flip_horizontal(&resized)
        } else {
            resized
        };

        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().max(1e-6);
        last = now;
        fps = if fps == 0.0 {
            1.0 / dt
        } else {
            0.9 * fps + 0.1 * (1.0 / dt)
        };

        buf.clear();
        buf.push_str("\x1b[H");
        render_frame(&resized, opts, mode, &mut buf);
        let _ = write!(
            buf,
            "\x1b[0m\x1b[J\r\nfps {fps:5.1}  {src_w}x{src_h}\u{2192}{w}x{h}px  [q/Esc to quit]\x1b[K",
            src_w = decoded.width(),
            src_h = decoded.height(),
        );
        out.write_all(buf.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

fn effective_mode(opts: &RenderOpts) -> Mode {
    if opts.no_color {
        Mode::Ascii
    } else {
        opts.mode
    }
}

/// Compute target image pixel dimensions for the requested mode, fitting
/// inside `max_w` × `max_h` terminal cells while preserving aspect ratio.
fn fit_image(
    src_w: u32,
    src_h: u32,
    max_w: u32,
    max_h: u32,
    mode: Mode,
    fill: bool,
) -> (u32, u32) {
    match mode {
        Mode::Blocks => {
            let (cell_w, cell_h) = if fill {
                (max_w, max_h)
            } else {
                let s_w = max_w as f32 / src_w as f32;
                let s_h = (max_h as f32 * 2.0) / src_h as f32;
                let s = s_w.min(s_h);
                let w = ((src_w as f32 * s).round() as u32).max(2);
                let h = ((src_h as f32 * s / 2.0).round() as u32).max(1);
                (w, h)
            };
            (cell_w.max(2), (cell_h.max(1)) * 2)
        }
        Mode::Ascii => {
            if fill {
                (max_w.max(1), max_h.max(1))
            } else {
                let s_w = max_w as f32 / src_w as f32;
                let s_h = (max_h as f32 * 2.0) / src_h as f32;
                let s = s_w.min(s_h);
                let w = ((src_w as f32 * s).round() as u32).max(1);
                let h = ((src_h as f32 * s / 2.0).round() as u32).max(1);
                (w, h)
            }
        }
    }
}

fn render_frame(img: &RgbImg, opts: &RenderOpts, mode: Mode, out: &mut String) {
    match mode {
        Mode::Blocks => render_blocks(img, opts, out),
        Mode::Ascii => render_ascii(img, opts, out),
    }
}

fn adjust(c: u8, brightness: f32, invert: bool) -> u8 {
    let mut v = (c as f32 / 255.0) * brightness;
    if invert {
        v = 1.0 - v;
    }
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn render_blocks(img: &RgbImg, opts: &RenderOpts, out: &mut String) {
    let w = img.width();
    let h = img.height();
    let mut y = 0;
    while y + 1 < h {
        let mut last: Option<((u8, u8, u8), (u8, u8, u8))> = None;
        for x in 0..w {
            let t = img.get_pixel(x, y);
            let b = img.get_pixel(x, y + 1);
            let tr = adjust(t[0], opts.brightness, opts.invert);
            let tg = adjust(t[1], opts.brightness, opts.invert);
            let tb = adjust(t[2], opts.brightness, opts.invert);
            let br = adjust(b[0], opts.brightness, opts.invert);
            let bg = adjust(b[1], opts.brightness, opts.invert);
            let bb = adjust(b[2], opts.brightness, opts.invert);
            let pair = ((tr, tg, tb), (br, bg, bb));
            if last != Some(pair) {
                let _ = write!(
                    out,
                    "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m"
                );
                last = Some(pair);
            }
            out.push('\u{2580}'); // ▀ upper half block
        }
        // \r\n (not just \n) — the terminal is in raw mode for camera, so a
        // bare LF doesn't return the cursor to col 0 and rows drift right.
        out.push_str("\x1b[0m\x1b[K\r\n");
        y += 2;
    }
}

fn render_ascii(img: &RgbImg, opts: &RenderOpts, out: &mut String) {
    let ramp_max = (RAMP.len() - 1) as f32;
    for y in 0..img.height() {
        let mut last_color: Option<(u8, u8, u8)> = None;
        for x in 0..img.width() {
            let p = img.get_pixel(x, y);
            let (r, g, b) = (p[0], p[1], p[2]);
            // Rec. 601 perceptual luminance
            let mut lum =
                (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0;
            lum = (lum * opts.brightness).clamp(0.0, 1.0);
            if opts.invert {
                lum = 1.0 - lum;
            }
            let idx = (lum * ramp_max).round() as usize;
            let ch = RAMP[idx] as char;

            if !opts.no_color && last_color != Some((r, g, b)) {
                let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
                last_color = Some((r, g, b));
            }
            out.push(ch);
        }
        if !opts.no_color {
            out.push_str("\x1b[0m");
        }
        // CR+LF for raw-mode camera; harmless in cooked-mode image output.
        out.push_str("\x1b[K\r\n");
    }
}
