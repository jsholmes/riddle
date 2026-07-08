//! riddle — the diary of Tom Riddle, for the reMarkable Paper Pro.
//!
//! Write on the page with the pen. After a pause the diary drinks your ink,
//! and an answer writes itself onto the page in a flowing hand, then fades.
//!
//! Two display backends (picked at runtime): windowed via qtfb/AppLoad when
//! QTFB_KEY is set, or full takeover via the vendor engine (quill) when
//! built with --features takeover and launched with xochitl stopped.

mod display;
mod fb;
mod help;
mod ink;
mod memory;
mod oracle;
mod pen;
mod power;
mod qtfb;
mod script;
mod surface;
mod touch;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ab_glyph::FontRef;

use fb::{BBox, SCREEN_H, SCREEN_W};
use oracle::Event;
use surface::{Surface, BLACK, FADED, WHITE};

const FONT_TTF: &[u8] = include_bytes!("../fonts/DancingScript.ttf");
const PNG_PATH: &str = "/tmp/riddle-page.png";

const IDLE_COMMIT: Duration = Duration::from_millis(2800);
/// How long the diary waits on a silent oracle before giving up on the turn.
/// Generous: thinking models can lead with a long silence.
const ORACLE_PATIENCE: Duration = Duration::from_secs(120);
/// How long the oracle may stay silent before the thinking blot starts
/// pulsing. A prompt reply arrives in stillness; the blot is for real waits.
const BLOT_PATIENCE: Duration = Duration::from_secs(4);
const REPLY_PX: f32 = 96.0;
const MARGIN_X: i32 = 120;
/// Where Tom's banter sits during a game: a strip near the page bottom,
/// clear of most boards. It is erased stroke-by-stroke (not rect-faded)
/// when the writer next moves, so a board crossing it survives.
const GAME_TEXT_Y: i32 = SCREEN_H as i32 - 430;

const USAGE: &str = "\
riddle — the diary of Tom Riddle

usage:
  riddle                      open the diary (windowed when AppLoad sets
                              QTFB_KEY, otherwise takeover via libquill)
  riddle --oracle-test [PNG]  run one oracle turn against PNG (default
                              /tmp/riddle-page.png) and print the streamed
                              reply; verifies key + endpoint + model
  riddle --draw-test [REPLY]  render a canned (or given) reply — prose and
                              ⟦ink:…⟧ sketches — through the real reply
                              pipeline to /tmp/riddle-draw-test.png
  riddle --draw-test game     simulate a scripted tic-tac-toe exchange
                              (board, moves, banter that erases between
                              turns) to /tmp/riddle-draw-test-game-*.png
  riddle --version            print the version

configuration lives in oracle.env next to the binary — see
oracle.env.example for every RIDDLE_* variable.
";

type OracleRx = mpsc::Receiver<Result<Event, String>>;

enum State {
    Listening { last_pen: Option<Instant> },
    Drinking { stage: u32, next: Instant, region: BBox, rx: OracleRx },
    /// `quiet`: a game turn — the page holds the board, so no thinking blot
    /// is ever stamped (its erase square would nick whatever it covered).
    Thinking { rx: OracleRx, pulse: Instant, blot_on: bool, since: Instant, quiet: bool },
    Replying { plan: WritePlan, next: Instant, rx: Option<OracleRx> },
    Lingering { until: Instant, region: BBox },
    FadingReply { stage: u32, next: Instant, region: BBox },
    /// The guide panel. `panel: None` = dismissed, waiting for pen-up so the
    /// dismissing touch doesn't leave a mark on the page.
    Help { panel: Option<help::Help>, until: Instant },
    /// A remembered page rising through the paper: date, the writer's own
    /// past ink, Tom's old reply — all in faded ink. `saved` is today's page.
    Conjuring { plan: ConjurePlan, next: Instant, saved: Vec<u8> },
    /// The conjured memory rests on the page. Pen contact (or time) dissolves
    /// it and today's page returns. `saved: None` = dismissed, waiting pen-up.
    MemoryShown { saved: Option<Vec<u8>>, until: Instant, region: BBox },
}

/// A memory being rewritten onto the page: pre-positioned strokes with their
/// original radii, drawn in faded ink.
struct ConjurePlan {
    strokes: Vec<Vec<(i32, i32, i32)>>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
}

struct WritePlan {
    strokes: Vec<Vec<(i32, i32)>>,
    /// is_prose[i]: stroke i is lettering. In game mode lettering is erased
    /// when the writer next moves, while drawing strokes (the moves) stay.
    is_prose: Vec<bool>,
    stroke_i: usize,
    point_i: usize,
    region: BBox,
    /// Where the next streamed chunk's first line starts.
    next_y: i32,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // Diagnostic: run one oracle turn and print the streamed chunks.
        // Lets you verify your endpoint + key + model before ever launching
        // the diary. No display needed.
        Some("--oracle-test") => {
            let png = args.get(2).map(String::as_str).unwrap_or(PNG_PATH);
            std::process::exit(oracle_test(png));
        }
        // Dev harness: feed a canned reply (prose + ⟦ink:…⟧ sketches)
        // through the real parse→plan→replay pipeline onto an offscreen
        // page and write PNG snapshots. No device, no display, no oracle.
        Some("--draw-test") => {
            if args.get(2).map(String::as_str) == Some("game") {
                std::process::exit(draw_test_game());
            }
            let reply = args.get(2).map(String::as_str).unwrap_or(
                "Here is a house. \u{27e6}ink: M 200,800 L 200,450 L 500,250 L 800,450 L 800,800 L 200,800 | M 500,800 L 500,600 L 620,600 L 620,800\u{27e7} Do you like it?",
            );
            std::process::exit(draw_test(reply));
        }
        Some("--version" | "-V") => {
            println!("riddle {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return;
        }
        Some(flag) if flag.starts_with('-') => {
            eprintln!("riddle: unknown flag {flag}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
        _ => {}
    }
    if let Err(e) = run() {
        eprintln!("riddle: fatal: {e}");
        std::process::exit(1);
    }
}

fn oracle_test(png: &str) -> i32 {
    let store = memory::MemoryStore::open();
    let o = match oracle::Oracle::spawn(store.is_some()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("oracle spawn failed: {e}");
            return 1;
        }
    };
    let ctx = build_ctx(&store);
    let (tx, rx) = mpsc::channel();
    let t0 = Instant::now();
    o.ask(png, &ctx, tx);
    let mut got = String::new();
    loop {
        match rx.recv() {
            Ok(Ok(Event::Ink(chunk))) => {
                if got.is_empty() {
                    eprintln!("first chunk +{}ms", t0.elapsed().as_millis());
                }
                print!("{chunk} ");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                got.push_str(&chunk);
            }
            Ok(Ok(Event::Show(id))) => {
                println!("[would conjure memory {id} — {}]", memory::spoken_date(id));
                got.push_str("(show)");
            }
            Ok(Ok(Event::Draw(polys))) => {
                let pts: usize = polys.iter().map(|p| p.len()).sum();
                println!("[would sketch: {} strokes, {} points]", polys.len(), pts);
                got.push_str("(sketch)");
            }
            Ok(Ok(Event::DrawPage(polys))) => {
                let pts: usize = polys.iter().map(|p| p.len()).sum();
                println!("[would draw on the page: {} strokes, {} points]", polys.len(), pts);
                got.push_str("(sketch)");
            }
            Ok(Ok(Event::Game(on))) => {
                println!("[game {}]", if on { "begins" } else { "ends" });
                got.push_str("(game)");
            }
            Ok(Ok(Event::Transcript(t))) => eprintln!("\n[transcript] {t}"),
            Ok(Err(e)) => {
                eprintln!("\noracle error: {e}");
                return 1;
            }
            Err(_) => break, // disconnected = reply complete
        }
    }
    println!("\n--- reply complete ({}ms, {} chars) ---", t0.elapsed().as_millis(), got.len());
    if got.trim().is_empty() { 1 } else { 0 }
}

/// Offscreen run of the reply pipeline: stream a canned reply through the
/// real parser, build the same WritePlan the diary would, replay every
/// stroke onto an in-memory page, and write it to /tmp/riddle-draw-test.png
/// (plus a mid-animation frame) for eyeballing. Exits 0 if anything inked.
fn draw_test(reply: &str) -> i32 {
    let font = match FontRef::try_from_slice(FONT_TTF) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("draw-test: font: {e}");
            return 1;
        }
    };
    let (w, h) = (SCREEN_W, SCREEN_H);
    let stride = w * 4;
    let mut buf = vec![0u8; stride * h];
    let mut surf = Surface::new(buf.as_mut_ptr(), buf.len(), w, h, stride, surface::PixFmt::Rgb32);
    surf.fill_rect(0, 0, w, h, WHITE);

    // Stream the reply in small slices to exercise the incremental parser.
    let mut parser = oracle::StreamParser::new(Vec::new());
    let mut events = Vec::new();
    let mut fed = String::new();
    for (i, ch) in reply.chars().enumerate() {
        fed.push(ch);
        if i % 7 == 0 {
            events.extend(parser.advance(&fed, false));
        }
    }
    events.extend(parser.advance(reply, true));

    let mut plan = empty_plan();
    let mut started = false;
    for ev in events {
        match ev {
            Ok(Event::Ink(t)) => {
                if started {
                    append_reply(&font, &mut plan, &t);
                } else {
                    plan = plan_reply(&font, &t, None);
                    started = true;
                }
            }
            Ok(Event::Draw(polys)) => {
                append_drawing(&mut plan, &polys);
                started = true;
            }
            Ok(Event::DrawPage(polys)) => {
                append_drawing_page(&mut plan, &polys);
                started = true;
            }
            Ok(Event::Game(on)) => {
                eprintln!("draw-test: game {}", if on { "begins" } else { "ends" });
            }
            Ok(Event::Show(_)) | Ok(Event::Transcript(_)) => {}
            Err(e) => eprintln!("draw-test: event error: {e}"),
        }
    }

    let dump = |buf: &[u8], name: &str| {
        dump_gray_png(buf, w, h, stride, &format!("/tmp/riddle-draw-test{name}.png"));
    };

    let total_points: usize = plan.strokes.iter().map(|s| s.len()).sum();
    let mut inked = 0usize;
    let mut mid_dumped = false;
    for stroke in &plan.strokes {
        for i in 0..stroke.len() {
            let (x, y) = stroke[i];
            if i > 0 {
                let (px, py) = stroke[i - 1];
                surf.brush_line(px, py, x, y, 2, BLACK);
            } else {
                surf.stamp(x, y, 2, BLACK);
            }
            inked += 1;
        }
        if !mid_dumped && inked >= total_points / 2 {
            dump(&buf, "-mid");
            mid_dumped = true;
        }
    }
    dump(&buf, "");
    // Also the page as a game-turn oracle would see it: ruled for aiming.
    match ink::page_to_png_ruled(&surf, "/tmp/riddle-draw-test-oracle.png") {
        Ok(()) => eprintln!("draw-test: wrote /tmp/riddle-draw-test-oracle.png (ruled oracle view)"),
        Err(e) => eprintln!("draw-test: ruled view: {e}"),
    }
    eprintln!(
        "draw-test: {} strokes, {} points, region {:?}",
        plan.strokes.len(),
        total_points,
        plan.region.rect()
    );
    if total_points > 0 { 0 } else { 1 }
}

/// Write an RGB32 page buffer as a grayscale PNG (dev harnesses only).
fn dump_gray_png(buf: &[u8], w: usize, h: usize, stride: usize, path: &str) {
    let mut gray = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            gray[y * w + x] = buf[y * stride + x * 4 + 2]; // R of B,G,R,FF
        }
    }
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("draw-test: {path}: {e}");
            return;
        }
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    match enc.write_header().and_then(|mut wr| wr.write_image_data(&gray)) {
        Ok(()) => eprintln!("draw-test: wrote {path}"),
        Err(e) => eprintln!("draw-test: {path}: {e}"),
    }
}

/// Offscreen multi-turn game simulation: a scripted "writer" draws a grid
/// and X's straight onto the page, and scripted Tom replies (⟦game⟧,
/// ⟦ink@page:…⟧ moves, banter, ⟦game over⟧) run through the real
/// parse→plan→replay path, with the previous turn's banter erased before
/// each writer move exactly as the game loop does. Writes one PNG per turn
/// to /tmp/riddle-draw-test-game-turn{N}.png for eyeballing.
fn draw_test_game() -> i32 {
    let font = match FontRef::try_from_slice(FONT_TTF) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("draw-test: font: {e}");
            return 1;
        }
    };
    let (w, h) = (SCREEN_W, SCREEN_H);
    let stride = w * 4;
    let mut buf = vec![0u8; stride * h];
    let mut surf = Surface::new(buf.as_mut_ptr(), buf.len(), w, h, stride, surface::PixFmt::Rgb32);
    surf.fill_rect(0, 0, w, h, WHITE);

    // The writer's hand: a 3x3 grid (300px cells) centered on the page.
    let cell = |cx: i32, cy: i32| (510 + 300 * cx, 780 + 300 * cy);
    for i in 1..3i32 {
        surf.brush_line(360 + 300 * i, 630, 360 + 300 * i, 1530, 3, BLACK);
        surf.brush_line(360, 630 + 300 * i, 1260, 630 + 300 * i, 3, BLACK);
    }

    // The writer plays X down the right column; Tom answers with page-
    // anchored O's (cell centers in thousandths: page center = 500,500),
    // then concedes. Directive coordinates map x/1620, y/2160 → 0–1000.
    let turns: [((i32, i32), &str); 3] = [
        (
            (2, 0),
            "\u{27e6}game\u{27e7} A challenge — very well, the first move was yours. \
             \u{27e6}ink@page: M 552,500 Q 552,539 500,539 Q 448,539 448,500 Q 448,461 500,461 Q 552,461 552,500\u{27e7} \
             The center suits me.",
        ),
        (
            (2, 1),
            "\u{27e6}ink@page: M 367,361 Q 367,400 315,400 Q 263,400 263,361 Q 263,322 315,322 Q 367,322 367,361\u{27e7} \
             You play with purpose.",
        ),
        ((2, 2), "Three in a line — the game is yours. \u{27e6}game over\u{27e7}"),
    ];

    let mut banter: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut errors = 0usize;
    for (i, ((cx, cy), reply)) in turns.iter().enumerate() {
        // The writer marks an X…
        let (x, y) = cell(*cx, *cy);
        surf.brush_line(x - 80, y - 80, x + 80, y + 80, 3, BLACK);
        surf.brush_line(x + 80, y - 80, x - 80, y + 80, 3, BLACK);
        // …the diary takes back Tom's previous banter (the commit step)…
        erase_strokes(&mut surf, &mut banter);
        // …and Tom replies through the real parser, streamed in slices.
        let mut parser = oracle::StreamParser::new(Vec::new());
        let mut events = Vec::new();
        let mut fed = String::new();
        for (j, ch) in reply.chars().enumerate() {
            fed.push(ch);
            if j % 7 == 0 {
                events.extend(parser.advance(&fed, false));
            }
        }
        events.extend(parser.advance(reply, true));

        let mut plan = empty_plan_at(GAME_TEXT_Y);
        for ev in events {
            match ev {
                Ok(Event::Ink(t)) => append_reply(&font, &mut plan, &t),
                Ok(Event::DrawPage(polys)) => append_drawing_page(&mut plan, &polys),
                Ok(Event::Draw(polys)) => append_drawing(&mut plan, &polys),
                Ok(Event::Game(on)) => {
                    eprintln!("draw-test: game {}", if on { "begins" } else { "ends" })
                }
                Ok(Event::Show(_)) | Ok(Event::Transcript(_)) => {}
                Err(e) => {
                    eprintln!("draw-test: event error: {e}");
                    errors += 1;
                }
            }
        }
        for stroke in &plan.strokes {
            for (k, &(sx, sy)) in stroke.iter().enumerate() {
                if k > 0 {
                    let (px, py) = stroke[k - 1];
                    surf.brush_line(px, py, sx, sy, 2, BLACK);
                } else {
                    surf.stamp(sx, sy, 2, BLACK);
                }
            }
        }
        banter = plan
            .strokes
            .iter()
            .zip(&plan.is_prose)
            .filter(|&(_, &p)| p)
            .map(|(s, _)| s.clone())
            .collect();
        dump_gray_png(&buf, w, h, stride, &format!("/tmp/riddle-draw-test-game-turn{}.png", i + 1));
    }
    if errors == 0 { 0 } else { 1 }
}

/// What the diary sends alongside the page: its memory of recent turns and
/// the catalog the oracle picks conjured pages from. Empty when memory is off.
fn build_ctx(store: &Option<memory::MemoryStore>) -> oracle::TurnContext {
    let Some(s) = store else { return oracle::TurnContext::default() };
    let turns: usize = std::env::var("RIDDLE_MEMORY_TURNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let (catalog_lines, catalog_ids) = s.catalog(40);
    oracle::TurnContext { history: s.recent_dialogue(turns), catalog_lines, catalog_ids }
}

fn run() -> std::io::Result<()> {
    let font = FontRef::try_from_slice(FONT_TTF).map_err(std::io::Error::other)?;

    let (disp, mut surf) = display::Display::open()?;
    let takeover = matches!(disp, display::Display::Quill);
    eprintln!(
        "riddle: display {} ({}x{} stride {})",
        if takeover { "quill/takeover" } else { "qtfb" },
        surf.w,
        surf.h,
        surf.stride
    );

    let mut pen_dev = match pen::PenDevice::open() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("riddle: raw pen unavailable ({e}), falling back to qtfb pen events");
            None
        }
    };
    // Takeover mode: touch is ours too; 5-finger tap = quit.
    let mut touch_dev = if takeover { touch::TouchDevice::open().ok() } else { None };
    // Takeover mode: the power button is ours too (sleep page + suspend).
    let mut power_dev = if takeover {
        power::PowerButton::open().map_err(|e| eprintln!("riddle: no power button ({e})")).ok()
    } else {
        None
    };
    // Ignore power presses briefly after a wake: the waking press itself (and
    // key bounce) arrives on our grabbed fd and must not re-suspend.
    let mut power_grace = Instant::now();

    let sigterm = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&sigterm))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&sigterm))?;

    // Blank page.
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    disp.update_all(surf.w, surf.h);

    // The diary's memory (None = RIDDLE_MEMORY=off or the dir is unusable).
    let mut store = memory::MemoryStore::open();
    if let Some(ref s) = store {
        eprintln!("riddle: memory holds {} pages", s.entries.len());
    }

    // Warm the oracle now: pi loads Node + extensions + codex auth ONCE here,
    // while you're still picking up the pen, so replies pay only model latency.
    let oracle = match oracle::Oracle::spawn(store.is_some()) {
        Ok(o) => {
            eprintln!("riddle: oracle ready");
            Some(o)
        }
        Err(e) => {
            eprintln!("riddle: oracle spawn failed: {e}");
            None
        }
    };

    let mut user_ink = ink::Ink::new();
    let mut state = State::Listening { last_pen: None };
    let mut pen_down = false;
    // The turn being remembered: strokes captured at commit, transcript and
    // reply accumulated as they stream, stored when the turn completes.
    let mut turn_id: u64 = 0;
    let mut turn_strokes: memory::Strokes = Vec::new();
    let mut turn_reply = String::new();
    let mut turn_transcript: Option<String> = None;
    let mut turn_failed = false;
    // Where this turn's ink lived (the writer's words, then the reply too):
    // the end-of-turn ghost-removal flash covers this instead of the panel.
    let mut turn_region = BBox::empty();
    // Drawn-game mode (⟦game⟧ … ⟦game over⟧). While on, the diary stops
    // drinking: the board persists and each commit sends the WHOLE page.
    let mut game_on = false;
    // ⟦game over⟧ arrived this turn: when the farewell finishes, the diary
    // drinks the entire page — board, marks and all.
    let mut game_ending = false;
    // Tom's banter strokes from the last game reply, erased (stroke by
    // stroke, so the board underneath survives) when the writer next moves.
    let mut game_banter: Vec<Vec<(i32, i32)>> = Vec::new();
    // Whether this turn's writer ink was drunk. If ⟦game⟧ arrives after a
    // drink (writer proposed and drew the board in one breath), the strokes
    // in turn_strokes are re-inked so the board comes back.
    let mut drank_this_turn = false;
    // Raw stylus contact, tracked in every state (the guide dismisses on it).
    // `stylus_on` is the level; `stylus_tapped` latches any contact seen this
    // loop iteration, so a tap that starts AND ends within one drain still
    // registers.
    let mut stylus_on = false;
    let mut stylus_tapped = false;
    let mut ink_dirty = BBox::empty();
    let mut last_flush = Instant::now();
    // Takeover swaps are cheap and synchronous; qtfb needs coalescing.
    let flush_every = if takeover { Duration::from_millis(8) } else { Duration::from_millis(35) };

    eprintln!("riddle: the diary is open");

    loop {
        if sigterm.load(Ordering::Relaxed) {
            break;
        }
        if let Some(ref mut t) = touch_dev {
            if t.drain_check_quit() {
                eprintln!("riddle: 5-finger quit");
                break;
            }
        }

        // ---- power button: sleep page, suspend, restore on wake ----
        if let Some(ref mut p) = power_dev {
            let pressed = p.drain_pressed();
            if pressed && Instant::now() >= power_grace {
                eprintln!("riddle: sleeping (power button)");
                let saved = help::show_sleep(&mut surf, &font);
                disp.full_refresh(surf.w, surf.h);
                // Let the flashing refresh finish before the panel loses power.
                std::thread::sleep(Duration::from_millis(800));
                // Discard key bounce from the initiating press; anything the
                // button says after this point means "never mind, wake up".
                p.drain_pressed();
                let outcome = power::suspend_until_wake(p);
                help::restore_sleep(&mut surf, &saved);
                disp.full_refresh(surf.w, surf.h);
                if outcome == power::SleepOutcome::Woke {
                    power::wifi_heal();
                }
                // Discard input that queued while asleep — stale pen events
                // would otherwise replay as phantom ink on the restored page.
                if let Some(ref mut pd) = pen_dev {
                    let _ = pd.drain();
                }
                if let Some(ref mut td) = touch_dev {
                    let _ = td.drain_check_quit();
                }
                p.drain_pressed();
                power_grace = Instant::now() + Duration::from_secs(3);
            }
        }

        // ---- raw pen (preferred path) ----
        if let Some(ref mut pdev) = pen_dev {
            for s in pdev.drain() {
                let writing = s.touching && s.pressure > 40;
                stylus_on = writing;
                stylus_tapped |= writing;
                if !writing {
                    if pen_down {
                        pen_down = false;
                        user_ink.pen_up();
                        if let State::Listening { ref mut last_pen } = state {
                            *last_pen = Some(Instant::now());
                        }
                    }
                    continue;
                }
                match state {
                    State::Listening { ref mut last_pen } => {
                        pen_down = true;
                        let d = match s.tool {
                            pen::Tool::Pen => {
                                let r = 2 + s.pressure * 3 / pen::MAX_PRESSURE;
                                user_ink.pen_point(&mut surf, s.x, s.y, r)
                            }
                            pen::Tool::Eraser => user_ink.erase_point(&mut surf, s.x, s.y, 22),
                        };
                        if !d.is_empty() {
                            ink_dirty.add(d.x0, d.y0, 0);
                            ink_dirty.add(d.x1, d.y1, 0);
                        }
                        *last_pen = Some(Instant::now());
                    }
                    State::Lingering { region, .. } => {
                        state = State::FadingReply { stage: 0, next: Instant::now(), region };
                    }
                    _ => {}
                }
            }
        }

        // ---- window-system events (qtfb close detection + pen fallback) ----
        let events = match disp.pump() {
            Ok(v) => v,
            Err(_) => break, // qtfb window closed
        };
        for ev in events {
            if pen_dev.is_some() {
                continue;
            }
            match ev.input_type {
                qtfb::INPUT_PEN_PRESS | qtfb::INPUT_PEN_UPDATE => {
                    stylus_on = true;
                    stylus_tapped = true;
                    if let State::Listening { ref mut last_pen } = state {
                        pen_down = true;
                        let r = 2 + ev.d.clamp(0, 100) / 45;
                        let d = user_ink.pen_point(&mut surf, ev.x, ev.y, r);
                        if !d.is_empty() {
                            ink_dirty.add(d.x0, d.y0, 0);
                            ink_dirty.add(d.x1, d.y1, 0);
                        }
                        *last_pen = Some(Instant::now());
                    } else if let State::Lingering { region, .. } = state {
                        state = State::FadingReply { stage: 0, next: Instant::now(), region };
                    }
                }
                qtfb::INPUT_PEN_RELEASE => {
                    stylus_on = false;
                    if pen_down {
                        pen_down = false;
                        user_ink.pen_up();
                        if let State::Listening { ref mut last_pen } = state {
                            *last_pen = Some(Instant::now());
                        }
                    }
                }
                _ => {}
            }
        }

        // ---- coalesced ink flush ----
        if !ink_dirty.is_empty() && last_flush.elapsed() >= flush_every {
            let (x, y, w, h) = ink_dirty.rect();
            disp.update(x, y, w, h, true);
            ink_dirty = BBox::empty();
            last_flush = Instant::now();
        }

        // ---- state machine ----
        state = match state {
            State::Listening { last_pen } => match last_pen {
                Some(t) if !pen_down && t.elapsed() >= IDLE_COMMIT && !user_ink.is_empty() => {
                    if region_all_white(&surf, user_ink.bbox) {
                        // Everything was erased before the pause: nothing to
                        // commit (and no phantom "?" from erased strokes).
                        user_ink.clear();
                        State::Listening { last_pen: None }
                    } else if !game_on && help::looks_like_question_mark(user_ink.stroke_list()) {
                        // Absorb the "?" and open the guide instead of asking.
                        let (qx, qy, qw, qh) = user_ink.bbox.rect();
                        surf.fill_rect(qx as usize, qy as usize, qw as usize, qh as usize, WHITE);
                        disp.update(qx, qy, qw, qh, false);
                        user_ink.clear();
                        let panel = help::show(&mut surf, &font, takeover);
                        let (px, py, pw, ph) = panel.region.rect();
                        disp.update(px, py, pw, ph, false);
                        eprintln!("riddle: guide shown");
                        State::Help { panel: Some(panel), until: Instant::now() + Duration::from_secs(45) }
                    } else if game_on && help::looks_like_question_mark(user_ink.stroke_list()) {
                        // The big "?" during a game puts the game away AT
                        // ONCE — local, instant, no waiting on a slow spirit.
                        let (qx, qy, qw, qh) = user_ink.bbox.rect();
                        surf.fill_rect(qx as usize, qy as usize, qw as usize, qh as usize, WHITE);
                        disp.update(qx, qy, qw, qh, false);
                        user_ink.clear();
                        game_on = false;
                        game_ending = false;
                        game_banter = Vec::new();
                        eprintln!("riddle: game ends (writer's ? gesture)");
                        State::Lingering { until: Instant::now(), region: full_page() }
                    } else if oracle.is_none() {
                        // No spirit at all: don't eat ink that nothing will
                        // answer — leave the writing and put the reason below.
                        let y = (user_ink.bbox.y1 + 90).min(SCREEN_H as i32 - 400);
                        let plan = plan_reply(&font, &oracle_excuse("no oracle"), Some(y));
                        State::Replying { plan, next: Instant::now(), rx: None }
                    } else if game_on {
                        // A move in a drawn game: nothing is drunk — the
                        // board must persist. Tom's previous banter is
                        // erased, then the WHOLE page goes to the oracle so
                        // it sees the board exactly as the writer does.
                        let erased = erase_strokes(&mut surf, &mut game_banter);
                        if !erased.is_empty() {
                            let (x, y, w, h) = erased.rect();
                            disp.update(x, y, w, h, false);
                        }
                        if let Err(e) = ink::page_to_png_ruled(&surf, PNG_PATH) {
                            eprintln!("riddle: rasterize failed: {e}");
                        }
                        turn_id = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        turn_strokes = user_ink.stroke_list().to_vec();
                        turn_reply.clear();
                        turn_transcript = None;
                        turn_failed = false;
                        drank_this_turn = false;
                        turn_region = BBox::empty();
                        let (tx, rx) = mpsc::channel();
                        if let Some(ref o) = oracle {
                            o.ask(PNG_PATH, &build_ctx(&store), tx);
                        }
                        if std::env::var_os("RIDDLE_KEEP_PAGE").is_none() {
                            let _ = std::fs::remove_file(PNG_PATH);
                        }
                        user_ink.clear();
                        State::Thinking {
                            rx,
                            pulse: Instant::now(),
                            blot_on: false,
                            since: Instant::now(),
                            quiet: true,
                        }
                    } else {
                        if let Err(e) = user_ink.to_png(&surf, PNG_PATH) {
                            eprintln!("riddle: rasterize failed: {e}");
                        }
                        // Remember this page: strokes now (they're cleared
                        // after the drink), transcript/reply as they stream.
                        turn_id = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        turn_strokes = user_ink.stroke_list().to_vec();
                        turn_reply.clear();
                        turn_transcript = None;
                        turn_failed = false;
                        // Ask NOW: the model streams while the diary drinks the
                        // ink, hiding most of the reply latency in the animation.
                        let (tx, rx) = mpsc::channel();
                        if let Some(ref o) = oracle {
                            o.ask(PNG_PATH, &build_ctx(&store), tx);
                        }
                        // Both backends read the page before ask() returns; the
                        // writer's words don't need to sit on disk afterwards.
                        if std::env::var_os("RIDDLE_KEEP_PAGE").is_none() {
                            let _ = std::fs::remove_file(PNG_PATH);
                        }
                        let region = user_ink.bbox;
                        turn_region = region;
                        drank_this_turn = true;
                        State::Drinking { stage: 0, next: Instant::now(), region, rx }
                    }
                }
                _ => State::Listening { last_pen },
            },

            State::Drinking { stage, next, region, rx } => {
                const STAGES: u32 = 14;
                if Instant::now() >= next {
                    ink::dissolve_pass(&mut surf, region, stage, STAGES);
                    let (x, y, w, h) = region.rect();
                    disp.update(x, y, w, h, true);
                    if stage + 1 >= STAGES {
                        user_ink.clear();
                        State::Thinking {
                            rx,
                            pulse: Instant::now(),
                            blot_on: false,
                            since: Instant::now(),
                            quiet: false,
                        }
                    } else {
                        State::Drinking { stage: stage + 1, next: Instant::now() + Duration::from_millis(70), region, rx }
                    }
                } else {
                    State::Drinking { stage, next, region, rx }
                }
            }

            State::Thinking { rx, pulse, blot_on, since, quiet } => match rx.try_recv() {
                Ok(result) => {
                    // Never touch the blot square once a game is on: ⟦game⟧
                    // may have just restored the board across page center,
                    // and this white patch would nick it.
                    if !quiet && !game_on {
                        surf.fill_rect(SCREEN_W / 2 - 14, SCREEN_H / 2 - 14, 28, 28, WHITE);
                        disp.update(SCREEN_W as i32 / 2 - 14, SCREEN_H as i32 / 2 - 14, 28, 28, true);
                    }
                    // First streamed event: start writing now; keep the
                    // receiver so the rest of the reply can append itself.
                    match result {
                        Ok(Event::Show(id)) => {
                            // An incantation: the rest of this turn is the
                            // conjured memory, not a reply. (rx drops here.)
                            match conjure(&font, &store, id, &mut surf, &disp) {
                                Some(st) => st,
                                None => {
                                    eprintln!("riddle: memory {id} is missing");
                                    let plan = plan_reply(&font, &oracle_excuse("lost page"), None);
                                    turn_failed = true;
                                    State::Replying { plan, next: Instant::now(), rx: None }
                                }
                            }
                        }
                        Ok(Event::Ink(text)) => {
                            turn_reply.push_str(&text);
                            let y = if game_on { Some(GAME_TEXT_Y) } else { None };
                            let plan = plan_reply(&font, &text, y);
                            State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                        }
                        Ok(Event::Draw(polys)) => {
                            // The reply opens with a sketch.
                            let mut plan = empty_plan();
                            append_drawing(&mut plan, &polys);
                            State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                        }
                        Ok(Event::DrawPage(polys)) => {
                            // The reply opens with a move on the board; any
                            // banter that follows goes to the game strip.
                            let mut plan =
                                if game_on { empty_plan_at(GAME_TEXT_Y) } else { empty_plan() };
                            append_drawing_page(&mut plan, &polys);
                            State::Replying { plan, next: Instant::now(), rx: Some(rx) }
                        }
                        Ok(Event::Game(on)) => {
                            eprintln!("riddle: game {}", if on { "begins" } else { "ends" });
                            if on && !game_on && drank_this_turn && !turn_strokes.is_empty() {
                                // The proposal and the board were drunk
                                // together this turn: give the ink back.
                                let r = restore_strokes(&mut surf, &turn_strokes);
                                if !r.is_empty() {
                                    let (x, y, w, h) = r.rect();
                                    disp.update(x, y, w, h, false);
                                }
                            }
                            if on {
                                // A (re)start cancels any pending end.
                                game_ending = false;
                            } else if game_on {
                                // Only a game that was on can end; a stray
                                // ⟦game over⟧ must not drink the page.
                                game_ending = true;
                            }
                            game_on = on;
                            State::Thinking { rx, pulse, blot_on, since, quiet }
                        }
                        Ok(Event::Transcript(t)) => {
                            // Transcript with no prose (model skipped the
                            // reply): remember the words, keep waiting.
                            if game_on && !game_ending && wants_to_stop(&t) {
                                eprintln!("riddle: game ends (writer asked; transcript failsafe)");
                                game_on = false;
                                game_ending = true;
                            }
                            turn_transcript = Some(t);
                            State::Thinking { rx, pulse, blot_on, since, quiet }
                        }
                        Err(e) => {
                            eprintln!("riddle: oracle failed: {e}");
                            turn_failed = true;
                            let y = if game_on { Some(GAME_TEXT_Y) } else { None };
                            let plan = plan_reply(&font, &oracle_excuse(&e), y);
                            State::Replying { plan, next: Instant::now(), rx: None }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if since.elapsed() >= ORACLE_PATIENCE {
                        // The oracle never answered (stalled stream, dead pi):
                        // stop pulsing and say so instead of thinking forever.
                        eprintln!("riddle: oracle timed out after {}s", ORACLE_PATIENCE.as_secs());
                        if !quiet {
                            surf.fill_rect(SCREEN_W / 2 - 14, SCREEN_H / 2 - 14, 28, 28, WHITE);
                            disp.update(SCREEN_W as i32 / 2 - 14, SCREEN_H as i32 / 2 - 14, 28, 28, true);
                        }
                        let y = if game_on { Some(GAME_TEXT_Y) } else { None };
                        let plan = plan_reply(&font, &oracle_excuse("timed out"), y);
                        State::Replying { plan, next: Instant::now(), rx: None }
                    } else if !quiet
                        && !game_on
                        && pulse.elapsed() >= Duration::from_millis(600)
                        && (blot_on || since.elapsed() >= BLOT_PATIENCE)
                    {
                        let (cx, cy) = (SCREEN_W as i32 / 2, SCREEN_H as i32 / 2);
                        if blot_on {
                            surf.fill_rect(cx as usize - 14, cy as usize - 14, 28, 28, WHITE);
                        } else {
                            surf.stamp(cx, cy, 9, BLACK);
                        }
                        disp.update(cx - 14, cy - 14, 28, 28, true);
                        // The blot's fast updates ghost; make sure the
                        // end-of-turn flash sweeps its patch too.
                        turn_region.add(cx - 14, cy - 14, 0);
                        turn_region.add(cx + 14, cy + 14, 0);
                        State::Thinking { rx, pulse: Instant::now(), blot_on: !blot_on, since, quiet }
                    } else {
                        State::Thinking { rx, pulse, blot_on, since, quiet }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    if game_ending {
                        // ⟦game over⟧ with no farewell: drink the page anyway.
                        game_ending = false;
                        game_banter = Vec::new();
                        State::Lingering {
                            until: Instant::now() + Duration::from_secs(3),
                            region: full_page(),
                        }
                    } else {
                        State::Listening { last_pen: None }
                    }
                }
            },

            State::Replying { mut plan, next, mut rx } => {
                // More of the reply may still be streaming in: append each
                // new chunk below what is already planned, mid-animation.
                if let Some(ref r) = rx {
                    let drop_rx = match r.try_recv() {
                        Ok(Ok(Event::Ink(more))) => {
                            if plan.next_y > SCREEN_H as i32 - 200 {
                                // The page is full: let the rest go unwritten
                                // rather than inking below the visible page.
                                eprintln!("riddle: reply reached the page bottom; trailing text dropped");
                                true
                            } else {
                                turn_reply.push_str(" ");
                                turn_reply.push_str(&more);
                                append_reply(&font, &mut plan, &more);
                                false
                            }
                        }
                        Ok(Ok(Event::Draw(polys))) => {
                            if plan.next_y > SCREEN_H as i32 - 360 {
                                eprintln!("riddle: page too full for the sketch");
                            } else {
                                append_drawing(&mut plan, &polys);
                            }
                            false
                        }
                        Ok(Ok(Event::DrawPage(polys))) => {
                            // Page-anchored: lands where the model aimed it,
                            // outside the prose flow — no room check needed.
                            append_drawing_page(&mut plan, &polys);
                            false
                        }
                        Ok(Ok(Event::Game(on))) => {
                            eprintln!("riddle: game {}", if on { "begins" } else { "ends" });
                            if on && !game_on && drank_this_turn && !turn_strokes.is_empty() {
                                let r = restore_strokes(&mut surf, &turn_strokes);
                                if !r.is_empty() {
                                    let (x, y, w, h) = r.rect();
                                    disp.update(x, y, w, h, false);
                                }
                            }
                            if on {
                                // A (re)start cancels any pending end.
                                game_ending = false;
                            } else if game_on {
                                // Only a game that was on can end; a stray
                                // ⟦game over⟧ must not drink the page.
                                game_ending = true;
                            }
                            game_on = on;
                            false
                        }
                        Ok(Ok(Event::Transcript(t))) => {
                            if game_on && !game_ending && wants_to_stop(&t) {
                                eprintln!("riddle: game ends (writer asked; transcript failsafe)");
                                game_on = false;
                                game_ending = true;
                            }
                            turn_transcript = Some(t);
                            false // the disconnect is still coming
                        }
                        Ok(Ok(Event::Show(_))) => {
                            eprintln!("riddle: conjuring directive mid-reply ignored");
                            false
                        }
                        Ok(Err(e)) => {
                            eprintln!("riddle: oracle failed mid-reply: {e}");
                            turn_failed = true;
                            true
                        }
                        Err(mpsc::TryRecvError::Disconnected) => true,
                        Err(mpsc::TryRecvError::Empty) => false,
                    };
                    if drop_rx {
                        rx = None;
                    }
                }
                if Instant::now() >= next {
                    let mut dirty = BBox::empty();
                    let mut budget = 26;
                    while budget > 0 && plan.stroke_i < plan.strokes.len() {
                        let stroke = &plan.strokes[plan.stroke_i];
                        if plan.point_i >= stroke.len() {
                            plan.stroke_i += 1;
                            plan.point_i = 0;
                            continue;
                        }
                        let (x, y) = stroke[plan.point_i];
                        if plan.point_i > 0 {
                            let (px, py) = stroke[plan.point_i - 1];
                            surf.brush_line(px, py, x, y, 2, BLACK);
                        } else {
                            surf.stamp(x, y, 2, BLACK);
                        }
                        dirty.add(x, y, 4);
                        plan.point_i += 1;
                        budget -= 1;
                    }
                    if !dirty.is_empty() {
                        let (x, y, w, h) = dirty.rect();
                        disp.update(x, y, w, h, true);
                    }
                    if plan.stroke_i >= plan.strokes.len() && rx.is_none() {
                        // The turn is complete: the diary remembers it —
                        // except game turns, which would only clutter the
                        // memory with board after board. (`game_ending`
                        // covers the final turn, whose ⟦game over⟧ already
                        // cleared `game_on`: its strokes are one lone move.)
                        if !turn_failed && !turn_reply.is_empty() && !game_on && !game_ending {
                            if let Some(ref mut s) = store {
                                s.append(
                                    turn_id,
                                    turn_transcript.as_deref().unwrap_or(""),
                                    turn_reply.trim(),
                                    &turn_strokes,
                                );
                            }
                        }
                        turn_strokes = Vec::new();
                        if game_on {
                            // The board stays; only Tom's lettering is kept
                            // aside, to be erased when the writer next moves.
                            game_banter = plan
                                .strokes
                                .iter()
                                .zip(&plan.is_prose)
                                .filter(|&(_, &p)| p)
                                .map(|(s, _)| s.clone())
                                .collect();
                            State::Listening { last_pen: None }
                        } else if game_ending {
                            // The game just ended: let the farewell rest,
                            // then the diary drinks the whole page — board,
                            // marks, banter and all.
                            game_ending = false;
                            game_banter = Vec::new();
                            State::Lingering {
                                until: Instant::now() + Duration::from_secs(6),
                                region: full_page(),
                            }
                        } else {
                            let chars: usize = plan.strokes.iter().map(|s| s.len()).sum();
                            let linger = Duration::from_millis(4000 + (chars as u64) * 2);
                            let region = plan.region;
                            State::Lingering { until: Instant::now() + linger.min(Duration::from_secs(20)), region }
                        }
                    } else {
                        State::Replying { plan, next: Instant::now() + Duration::from_millis(14), rx }
                    }
                } else {
                    State::Replying { plan, next, rx }
                }
            }

            State::Lingering { until, region } => {
                if Instant::now() >= until {
                    State::FadingReply { stage: 0, next: Instant::now(), region }
                } else {
                    State::Lingering { until, region }
                }
            }

            State::Help { panel, until } => match panel {
                Some(p) => {
                    if stylus_tapped || Instant::now() >= until {
                        let region = p.dismiss(&mut surf);
                        let (x, y, w, h) = region.rect();
                        disp.update(x, y, w, h, false);
                        eprintln!("riddle: guide dismissed");
                        State::Help { panel: None, until }
                    } else {
                        State::Help { panel: Some(p), until }
                    }
                }
                // Dismissed: swallow the closing touch, listen again on pen-up.
                None if stylus_on => State::Help { panel: None, until },
                None => State::Listening { last_pen: None },
            },

            State::Conjuring { mut plan, next, saved } => {
                if stylus_tapped {
                    // The writer interrupts: today's page returns at once.
                    surf.paste_rect(0, 0, SCREEN_W, SCREEN_H, &saved);
                    disp.full_refresh(surf.w, surf.h);
                    turn_region = BBox::empty();
                    State::MemoryShown { saved: None, until: Instant::now(), region: plan.region }
                } else if Instant::now() >= next {
                    // The memory pours back faster than Tom writes: it is
                    // remembered, not composed.
                    let mut dirty = BBox::empty();
                    let mut budget = 48;
                    while budget > 0 && plan.stroke_i < plan.strokes.len() {
                        let stroke = &plan.strokes[plan.stroke_i];
                        if plan.point_i >= stroke.len() {
                            plan.stroke_i += 1;
                            plan.point_i = 0;
                            continue;
                        }
                        let (x, y, r) = stroke[plan.point_i];
                        if plan.point_i > 0 {
                            let (px, py, pr) = stroke[plan.point_i - 1];
                            surf.brush_line(px, py, x, y, r.min(pr + 1), FADED);
                        } else {
                            surf.stamp(x, y, r, FADED);
                        }
                        dirty.add(x, y, r + 2);
                        plan.point_i += 1;
                        budget -= 1;
                    }
                    if !dirty.is_empty() {
                        let (x, y, w, h) = dirty.rect();
                        disp.update(x, y, w, h, true);
                    }
                    if plan.stroke_i >= plan.strokes.len() {
                        let region = plan.region;
                        State::MemoryShown {
                            saved: Some(saved),
                            until: Instant::now() + Duration::from_secs(120),
                            region,
                        }
                    } else {
                        State::Conjuring { plan, next: Instant::now() + Duration::from_millis(10), saved }
                    }
                } else {
                    State::Conjuring { plan, next, saved }
                }
            }

            State::MemoryShown { saved, until, region } => match saved {
                Some(s) => {
                    if stylus_tapped || Instant::now() >= until {
                        // The paper swallows its memory; today's page returns.
                        surf.paste_rect(0, 0, SCREEN_W, SCREEN_H, &s);
                        disp.full_refresh(surf.w, surf.h);
                        turn_region = BBox::empty();
                        eprintln!("riddle: memory dismissed");
                        State::MemoryShown { saved: None, until, region }
                    } else {
                        State::MemoryShown { saved: Some(s), until, region }
                    }
                }
                // Dismissed: swallow the closing touch, listen again on pen-up.
                None if stylus_on => State::MemoryShown { saved: None, until, region },
                None => State::Listening { last_pen: None },
            },

            State::FadingReply { stage, next, region } => {
                // The reply dissolves exactly as the writer's ink does when
                // the diary drinks it: same stages, same pace.
                const STAGES: u32 = 14;
                if Instant::now() >= next {
                    ink::dissolve_pass(&mut surf, region, stage, STAGES);
                    let (x, y, w, h) = region.rect();
                    disp.update(x, y, w, h, true);
                    if stage + 1 >= STAGES {
                        // Ghost removal flashes only where this turn's ink
                        // lived — the writer's words and the reply — not the
                        // whole panel.
                        if !region.is_empty() {
                            turn_region.add(region.x0, region.y0, 0);
                            turn_region.add(region.x1, region.y1, 0);
                        }
                        if !turn_region.is_empty() {
                            let (x, y, w, h) = turn_region.rect();
                            disp.flash(x, y, w, h);
                        }
                        turn_region = BBox::empty();
                        State::Listening { last_pen: None }
                    } else {
                        State::FadingReply { stage: stage + 1, next: Instant::now() + Duration::from_millis(70), region }
                    }
                } else {
                    State::FadingReply { stage, next, region }
                }
            }
        };

        stylus_tapped = false;
        std::thread::sleep(Duration::from_millis(2));
    }

    eprintln!("riddle: the diary closes");
    disp.terminate();
    Ok(())
}

/// True if the region no longer holds any dark pixels (fully erased).
fn region_all_white(surf: &Surface, region: BBox) -> bool {
    if region.is_empty() {
        return true;
    }
    for y in region.y0..=region.y1 {
        for x in region.x0..=region.x1 {
            if surf.luma(x, y) < 200 {
                return false;
            }
        }
    }
    true
}

/// What Tom writes when the spirit cannot answer: short, in a diary's voice,
/// but specific enough to act on. The raw error still goes to stderr.
fn oracle_excuse(e: &str) -> String {
    if e.contains("no oracle") {
        "The diary lies dormant: it found no oracle. \
         Put an API key in oracle.env, then open me again."
            .into()
    } else if e.starts_with("http 401") || e.starts_with("http 403") {
        "The oracle refused the diary's key. Check RIDDLE_OPENAI_KEY in oracle.env.".into()
    } else if e.starts_with("http ") {
        let code = e.split(':').next().unwrap_or("an error");
        format!("The oracle rejected the diary's plea ({code}). Check the model and endpoint in oracle.env.")
    } else if e.contains("request failed") || e.contains("timed out") {
        "The diary cannot reach its oracle. Is the tablet connected to Wi-Fi?".into()
    } else if e.contains("empty reply") {
        "The spirit read your words but said nothing. Write again.".into()
    } else {
        "The ink blurred before it could answer. Write again.".into()
    }
}

/// Summon a remembered page: snapshot today's page, clear the paper, and plan
/// the memory's rewriting — the date in a small hand, the writer's own strokes
/// exactly as they were penned, Tom's old reply beneath — all in faded ink.
fn conjure(
    font: &FontRef,
    store: &Option<memory::MemoryStore>,
    id: u64,
    surf: &mut Surface,
    disp: &display::Display,
) -> Option<State> {
    let s = store.as_ref()?;
    let entry = s.get(id)?.clone();
    let strokes = s.strokes(id).unwrap_or_default();
    eprintln!("riddle: conjuring memory {id} ({})", memory::spoken_date(id));

    let saved = surf.copy_rect(0, 0, SCREEN_W, SCREEN_H);
    surf.fill_rect(0, 0, SCREEN_W, SCREEN_H, WHITE);
    disp.update_all(surf.w, surf.h);

    let mut all: Vec<Vec<(i32, i32, i32)>> = Vec::new();
    let mut region = BBox::empty();

    // The date, small and centered near the top, like a diary heading.
    let date = memory::spoken_date(entry.id);
    let mut raster = script::rasterize_line(font, &date, 54.0);
    script::thin(&mut raster);
    let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
    let mut ink_bottom = 64;
    for stroke in script::trace(&raster) {
        let mapped: Vec<(i32, i32, i32)> =
            stroke.iter().map(|&(sx, sy)| (x0 + sx, 64 + sy, 1)).collect();
        for &(x, y, r) in &mapped {
            region.add(x, y, r + 2);
            ink_bottom = ink_bottom.max(y);
        }
        all.push(mapped);
    }

    // The writer's own hand, exactly as it was penned.
    for stroke in &strokes {
        for &(x, y, r) in stroke {
            region.add(x, y, r + 2);
            ink_bottom = ink_bottom.max(y);
        }
        all.push(stroke.clone());
    }

    // Tom's old reply, below.
    if !entry.reply.is_empty() {
        let y = (ink_bottom + 130).min(SCREEN_H as i32 - 400);
        let reply = plan_reply(font, &entry.reply, Some(y));
        for stroke in reply.strokes {
            let mapped: Vec<(i32, i32, i32)> = stroke.iter().map(|&(x, y)| (x, y, 2)).collect();
            for &(x, y, r) in &mapped {
                region.add(x, y, r + 2);
            }
            all.push(mapped);
        }
    }

    Some(State::Conjuring {
        plan: ConjurePlan { strokes: all, stroke_i: 0, point_i: 0, region },
        next: Instant::now(),
        saved,
    })
}

/// Lay out reply text and produce screen-space strokes. `y_start` continues a
/// streamed reply below its previous chunk; None places the first chunk.
fn plan_reply(font: &FontRef, text: &str, y_start: Option<i32>) -> WritePlan {
    let max_w = (SCREEN_W as i32 - 2 * MARGIN_X) as f32;
    let lines = script::wrap(font, text, REPLY_PX, max_w);
    let line_h = (REPLY_PX * 1.25) as i32;
    let total_h = line_h * lines.len() as i32;
    let mut y = y_start.unwrap_or(((SCREEN_H as i32 - total_h) / 3).max(60));
    let mut strokes = Vec::new();
    let mut region = BBox::empty();
    let mut seed = 0x1234u32;
    let mut jitter = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((seed >> 16) % 7) as i32 - 3
    };

    for line_text in &lines {
        let mut raster = script::rasterize_line(font, line_text, REPLY_PX);
        script::thin(&mut raster);
        let line_strokes = script::trace(&raster);
        let x0 = (SCREEN_W as i32 - raster.width as i32) / 2;
        let wobble = jitter();
        for s in line_strokes {
            let mapped: Vec<(i32, i32)> = s.iter().map(|&(sx, sy)| (x0 + sx, y + sy + wobble)).collect();
            for &(x, yy) in &mapped {
                region.add(x, yy, 5);
            }
            strokes.push(mapped);
        }
        y += line_h;
    }

    let is_prose = vec![true; strokes.len()];
    WritePlan { strokes, is_prose, stroke_i: 0, point_i: 0, region, next_y: y }
}

/// Resample decoded 0–1000 polylines to pen-point spacing under `map` and
/// add them to the plan as drawing (non-prose) strokes.
fn splice_polys(
    plan: &mut WritePlan,
    polys: &[Vec<(f32, f32)>],
    map: impl Fn(f32, f32) -> (i32, i32),
) {
    const STEP: f32 = 2.5; // spacing between replayed points
    // The decode cap bounds the model's POINTS, but resampling multiplies
    // them: one page-diagonal segment becomes ~1000 replayed points. Cap the
    // whole plan so a confused model's zigzag stays a few seconds of quill,
    // not hours.
    const MAX_SPLICED: usize = 60_000;
    let mut total: usize = plan.strokes.iter().map(|s| s.len()).sum();
    for poly in polys {
        if poly.len() < 2 {
            continue;
        }
        if total >= MAX_SPLICED {
            eprintln!("riddle: drawing truncated at {total} points");
            break;
        }
        let mut stroke: Vec<(i32, i32)> = Vec::new();
        let mut last = map(poly[0].0, poly[0].1);
        stroke.push(last);
        for &(px, py) in &poly[1..] {
            let (tx, ty) = map(px, py);
            let (dx, dy) = ((tx - last.0) as f32, (ty - last.1) as f32);
            let steps = ((dx * dx + dy * dy).sqrt() / STEP).ceil().max(1.0) as i32;
            for i in 1..=steps {
                let t = i as f32 / steps as f32;
                stroke.push((last.0 + (dx * t) as i32, last.1 + (dy * t) as i32));
            }
            last = (tx, ty);
        }
        stroke.truncate(MAX_SPLICED - total);
        if stroke.len() < 2 {
            continue;
        }
        total += stroke.len();
        for &(x, y) in &stroke {
            plan.region.add(x, y, 5);
        }
        plan.is_prose.push(false);
        plan.strokes.push(stroke);
    }
}

/// Splice a sketch into the write animation: the model's 0–1000 square is
/// scaled into a box below what's written and resampled to pen-point spacing
/// so the quill draws it at handwriting pace.
fn append_drawing(plan: &mut WritePlan, polys: &[Vec<(f32, f32)>]) {
    const SIDE: i32 = 760; // the sketch box's edge, in pixels
    let x0 = (SCREEN_W as i32 - SIDE) / 2;
    let y0 = (plan.next_y + 40).min(SCREEN_H as i32 - SIDE - 60).max(60);
    let scale = SIDE as f32 / 1000.0;
    splice_polys(plan, polys, |px, py| (x0 + (px * scale) as i32, y0 + (py * scale) as i32));
    plan.next_y = y0 + SIDE + 40;
}

/// Splice a page-anchored sketch (⟦ink@page:…⟧): the model's 0–1000 square
/// maps to the WHOLE visible page — x in thousandths of the width, y of the
/// height — so a game move lands inside the cell the model saw in the
/// committed page image. Lives outside the prose flow: next_y is untouched.
fn append_drawing_page(plan: &mut WritePlan, polys: &[Vec<(f32, f32)>]) {
    let (sx, sy) = (SCREEN_W as f32 / 1000.0, SCREEN_H as f32 / 1000.0);
    splice_polys(plan, polys, move |px, py| ((px * sx) as i32, (py * sy) as i32));
}

/// Does the writer's transcribed page read as "stop the game"? A local
/// failsafe behind the model's own ⟦game over⟧: the writer's wish to stop
/// must never hang on the spirit's cooperation (or a slow turn). Phrases
/// match on word boundaries, and a nearby negation ("I do NOT want to stop
/// playing!") keeps the game alive — a missed stop still has the writer's
/// "?" gesture; a false stop drinks their board.
fn wants_to_stop(transcript: &str) -> bool {
    const PHRASES: [&str; 12] = [
        "stop playing", "stop the game", "stop this game", "quit the game",
        "let's stop", "lets stop", "done playing", "no more game",
        "no more games", "enough of this game", "end the game", "end this game",
    ];
    const NEGATIONS: [&str; 5] = ["don't", "dont", "do not", "never", "not"];
    let t = transcript.to_lowercase();
    for p in PHRASES {
        let mut from = 0;
        while let Some(rel) = t[from..].find(p) {
            let i = from + rel;
            let end = i + p.len();
            let boundary = |b: Option<&u8>| !b.is_some_and(|c| c.is_ascii_alphanumeric());
            if boundary(t.as_bytes().get(i.wrapping_sub(1)).filter(|_| i > 0))
                && boundary(t.as_bytes().get(end))
            {
                // Look a few words back for a negation.
                let mut lead = i.saturating_sub(32);
                while !t.is_char_boundary(lead) {
                    lead += 1;
                }
                if !NEGATIONS.iter().any(|n| t[lead..i].contains(n)) {
                    return true;
                }
            }
            from = end;
        }
    }
    false
}

/// A plan with nothing in it yet, opening at `y` — the starting point when a
/// drawing (not prose) leads the reply.
fn empty_plan_at(y: i32) -> WritePlan {
    WritePlan {
        strokes: Vec::new(),
        is_prose: Vec::new(),
        stroke_i: 0,
        point_i: 0,
        region: BBox::empty(),
        next_y: y,
    }
}

/// An empty plan at the reply's default height.
fn empty_plan() -> WritePlan {
    empty_plan_at((SCREEN_H as i32) / 4)
}

/// The whole visible page as a region (the game-over drink).
fn full_page() -> BBox {
    let mut b = BBox::empty();
    b.add(0, 0, 0);
    b.add(SCREEN_W as i32 - 1, SCREEN_H as i32 - 1, 0);
    b
}

/// Erase previously-replayed strokes by brushing white back over them —
/// surgical, so a game board crossing the same area survives where a
/// rect-fade would not. Radius 4 out-brushes the radius-2 quill. Clears the
/// stroke list (a second call is a no-op) and returns the touched region.
fn erase_strokes(surf: &mut Surface, strokes: &mut Vec<Vec<(i32, i32)>>) -> BBox {
    let mut region = BBox::empty();
    for stroke in strokes.iter() {
        for (i, &(x, y)) in stroke.iter().enumerate() {
            if i > 0 {
                let (px, py) = stroke[i - 1];
                surf.brush_line(px, py, x, y, 4, WHITE);
            } else {
                surf.stamp(x, y, 4, WHITE);
            }
            region.add(x, y, 6);
        }
    }
    strokes.clear();
    region
}

/// Re-ink writer strokes that were drunk earlier this turn — the writer
/// proposed a game and drew the board in the same breath, and ⟦game⟧ means
/// the diary must give the board back. Returns the touched region.
fn restore_strokes(surf: &mut Surface, strokes: &[Vec<(i32, i32, i32)>]) -> BBox {
    let mut region = BBox::empty();
    for stroke in strokes {
        for (i, &(x, y, r)) in stroke.iter().enumerate() {
            if i > 0 {
                let (px, py, pr) = stroke[i - 1];
                surf.brush_line(px, py, x, y, r.min(pr + 1), BLACK);
            } else {
                surf.stamp(x, y, r, BLACK);
            }
            region.add(x, y, r + 2);
        }
    }
    region
}

/// Splice a streamed continuation chunk into a running write animation.
fn append_reply(font: &FontRef, plan: &mut WritePlan, more: &str) {
    let cont = plan_reply(font, more, Some(plan.next_y));
    if cont.strokes.is_empty() {
        return;
    }
    plan.region.add(cont.region.x0, cont.region.y0, 0);
    plan.region.add(cont.region.x1, cont.region.y1, 0);
    plan.is_prose.extend(cont.is_prose);
    plan.strokes.extend(cont.strokes);
    plan.next_y = cont.next_y;
}

#[cfg(test)]
mod splice_tests {
    use super::{append_drawing_page, empty_plan};

    #[test]
    fn resampled_drawing_is_bounded() {
        // A page-spanning zigzag: 2000 decoded points, each segment nearly
        // the panel diagonal — unbounded resampling would make millions.
        let poly: Vec<(f32, f32)> = (0..2000)
            .map(|i| (if i % 2 == 0 { 0.0 } else { 1000.0 }, (i % 1000) as f32))
            .collect();
        let mut plan = empty_plan();
        append_drawing_page(&mut plan, &[poly]);
        let total: usize = plan.strokes.iter().map(|s| s.len()).sum();
        assert!(total <= 60_000, "resample must stay bounded, got {total}");
        assert!(total >= 2, "the drawing should not vanish entirely");
    }
}

#[cfg(test)]
mod stop_tests {
    use super::wants_to_stop;

    #[test]
    fn stop_phrases_end_games_and_chatter_does_not() {
        assert!(wants_to_stop("Let's stop playig this."));
        assert!(wants_to_stop("Can we STOP THE GAME now"));
        assert!(wants_to_stop("I'm done playing, Tom."));
        assert!(wants_to_stop("please, no more games"));
        assert!(!wants_to_stop("Don't stop now, your move!"));
        assert!(!wants_to_stop("I will play the top corner"));
    }

    #[test]
    fn enthusiasm_and_negation_do_not_end_the_game() {
        // A wish to KEEP playing must never drink the board.
        assert!(!wants_to_stop("I don't want to stop playing!"));
        assert!(!wants_to_stop("I never want to stop playing"));
        assert!(!wants_to_stop("do not stop the game"));
        // Word boundaries: gameplay banter around the words is not a plea.
        assert!(!wants_to_stop("my unstoppable gameplan"));
    }
}
