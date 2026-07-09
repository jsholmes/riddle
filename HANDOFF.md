# Handoff: the enchanted diary project (remagic + riddle)

*Written 2026-07-08 by Claude (Claude Code session with John). Audience: any
agent or human picking up this work — everything below is verified from this
session, not aspirational.*

## What this is

John runs a **reMarkable Paper Pro** (Ferrari, aarch64, OS 3.27.3.0) opened up
via developer mode. Two sibling repos:

- **remagic** (`~/Documents/GitHub/remagic`) — the installer/distribution
  layer: a pure-Go CLI (`remagic setup/install/config/doctor/wifi/publish`),
  a checksummed app catalog, and the on-device Store. It installs the modding
  stack: **xovi** (function-hooking loader injected into xochitl, the stock
  UI) → **AppLoad** (app launcher) → apps. Persistence is a power-button
  triple-press (xovi-tripletap); any reboot returns to stock — by design,
  nothing can bootloop the device.
- **riddle** (`~/Documents/GitHub/riddle`) — the flagship app: *the diary of
  Tom Riddle*. Write with the pen; after a 2.8 s pause the diary "drinks"
  your ink, sends the page as a PNG to a vision LLM ("the oracle"), and the
  reply writes itself back stroke-by-stroke in Dancing Script, then fades.
  Subdirs: `riddle/` (the Rust app), `quill/` (C/C++ takeover display host —
  interposes the vendor `libqsgepaper.so` e-ink engine for instant ink),
  `drawlab/`, `home/` (small app bundles).

**The human context matters:** the diary is (at least partly) for John's
daughter **Addie**. John doles features out over time, like a slowly
deepening magical artifact. Tone and game difficulty should assume a child
may be the writer. Tom's persona must never break character (never mentions
AI, models, images).

## How riddle works (one page's journey)

1. **Pen input** — raw evdev (`/dev/input/event2`), full 4096-level pressure,
   grabbed exclusively. Eraser = flip the marker. Touch = 5-finger tap quits
   (takeover). Power button = sleep page + suspend (takeover).
2. **Commit** — 2.8 s pen-idle (`IDLE_COMMIT`) → page rasterized to grayscale
   PNG (≤800 px long side), deleted after the oracle reads it.
3. **The oracle** (`riddle/src/oracle.rs`) — two backends behind one `ask()`:
   - **HTTP**: any OpenAI-compatible `/chat/completions` endpoint; selected
     when `RIDDLE_OPENAI_KEY` is set. Stateless; riddle resends recent
     history itself.
   - **pi** (current): resident `pi --mode rpc` process (Node at
     `/home/root/node/bin`), subscription auth from `/home/root/.pi`.
     Selected when no API key is set. pi holds the live conversation
     (context grows all session — see Known Issues #3).
   The persona + protocols are `const` strings in `oracle.rs` — changing
   them requires a rebuild. The model does everything: reads handwriting
   (no OCR on device), replies in character, and emits in-band directives.
4. **The directive protocol** (parsed by `StreamParser`, streamed
   sentence-by-sentence so the quill starts before the model finishes):
   - `⟦show:N⟧` — conjure remembered page N from the catalog.
   - `⁂ <transcript>` — hidden postscript: the model transcribes the
     writer's words; riddle stores it (the LLM *is* the OCR). Never inked.
   - `⟦ink: M x,y L x,y Q cx,cy x,y | …⟧` — a sketch (0–1000 square, scaled
     into a box below the prose). Animated by the same stroke-replay engine
     as handwriting.
   - `⟦game⟧` / `⟦game over⟧` — game mode on/off.
   - `⟦ink@page: …⟧` — page-anchored strokes (0–1000 maps to the whole page;
     x/y scales differ, page is 1620×2160) — used for game moves.
5. **Rendering** — `WritePlan` stroke queue replayed ~26 points/tick, radius
   2, via `Surface` (pixel buffer) → `Display` (qtfb windowed, or quill
   takeover: `quill_swap(x,y,w,h,mode,full)`; mode 0 fast ink, 3 balanced,
   4+full=1 flashing ghost-removal).
6. **Memory** (`memory.rs`) — every finished page (strokes, transcript,
   reply) stored under `/home/root/riddle-data/memories` (~400 pages max).
   Per turn the oracle gets the last 6 exchanges (`RIDDLE_MEMORY_TURNS`) +
   a 40-entry catalog of dated gists.

## Device access (hard-won gotchas)

- Addresses: **USB `10.11.99.1`**, **Wi-Fi `192.168.36.75`**. Passwordless
  SSH via `~/.ssh/id_ed25519`; always use
  `ssh -o IdentitiesOnly=yes -i ~/.ssh/id_ed25519 root@<addr>`.
- **1Password gotcha**: the remagic CLI's Go SSH hangs on John's machine
  unless the agent is bypassed — run it as `SSH_AUTH_SOCK= remagic …`.
- **Battery radio naps**: on battery the tablet drops Wi-Fi when idle and
  wakes it on demand. SSH over Wi-Fi only works while the screen is awake
  (or on charger). First oracle call after a long idle can lose the
  reconnect race — a retry works. USB always works when plugged.
- Tablet shell is BusyBox: no `timeout`, `head -N` needs `-n N`.
- Escape hatches: 5-finger tap exits the diary (systemd stop hook always
  restarts xochitl); triple-press toggles xovi; worst case
  `ssh root@10.11.99.1 'systemctl start xochitl'`; reboot = stock.

## Build & deploy (macOS, no rM SDK needed)

The official flow wants the reMarkable SDK; we replaced it with **zig**:

```sh
cd ~/Documents/GitHub/riddle/riddle
cargo test                      # 36 tests, runs natively on macOS
RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined" \
  cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.38 --features takeover
patchelf --replace-needed \
  /Users/john/Documents/GitHub/riddle/riddle/../quill/build/libquill.so \
  libquill.so target/aarch64-unknown-linux-gnu/release/riddle
```

- Vendor libs already pulled from the device into `quill/build/libquill.so`
  and `quill/vendor/libqsgepaper.so` (re-pull after OS updates).
- The `patchelf` step is REQUIRED: libquill.so has no SONAME, so lld embeds
  the absolute Mac path; without the fix the binary won't load on device
  (we hit this — symptom: "error while loading shared libraries: /Users/…").
- Deploy: scp to `…/appload/riddle/riddle.new`, smoke-test on device
  (`LD_LIBRARY_PATH=".:/home/root/quill:/usr/lib/plugins/scenegraph"
  ./riddle.new --version`), then `mv` over `riddle`. A known-good backup
  lives at `riddle.orig-0.3.0` on the device. A running diary keeps the old
  binary until relaunched (AppLoad → The Diary; 5-finger tap out first).

**Test rigs (use them before deploying):**
- `cargo test` — parser, path decode, stop-phrases, memory, etc.
- `cargo run -- --draw-test "<reply with ⟦ink:…⟧>"` — offscreen render of
  the full parse→plan→replay pipeline to `/tmp/riddle-draw-test.png`, plus
  `…-oracle.png` (the ruled game view). Runs on macOS. Look at the PNGs.
- `cargo run -- --draw-test game` — scripted 3-turn tic-tac-toe sim.
- Real-model evals: the tablet's pi is authenticated; run
  `pi --provider anthropic --model claude-sonnet-5 --no-tools --no-session
  --system-prompt "$(cat sys.txt)" -p @board.png "…"` over SSH on the
  tablet. (Local Mac pi has codex auth but NOT anthropic auth.)
  gpt tests can run locally via curl with John's OpenAI key.

## Current deployed state (tablet, as of 2026-07-08 evening)

Custom build from branch `fix/game-aim` containing, in order of arrival:
1. **Fade/blot polish** — reply dissolves exactly like writer ink (14×70 ms),
   ghost-removal flash confined to the turn's ink region (`turn_region`),
   thinking blot only after 4 s of oracle silence (`BLOT_PATIENCE`).
2. **Drawings** — the `⟦ink:…⟧` sketch directive (see protocol above).
3. **Games** (built by a parallel agent session) — `⟦game⟧` mode: ink stops
   fading, whole page sent per turn, Tom moves via `⟦ink@page:…⟧`, banter in
   a bottom strip erased between turns.
4. **Game-aim fix** — game pages sent to the oracle carry a faint ruler
   (gridline every 100 units, digits on top/left edges; `page_to_png_ruled`
   in `ink.rs`) + a worked measurement example in `GAME_PROTOCOL`. Eval on
   the real Sonnet oracle, 6 trials, small off-center board: 4 clean in-cell
   moves, 2 near-misses at cell edges, 0 wild misses (before: played page
   center regardless of board).
5. **Game exits** — big "?" during a game ends it instantly (local, no
   model); transcript failsafe ends it when the writer's words say stop
   (`wants_to_stop`, unit-tested); Tom still does his own `⟦game over⟧`.
   Guide panel documents all of it.
6. **Drawing quality pass** (after reviewing Addie's actual gallery via the
   pi session log + offscreen re-renders): `decode_ink` now supports `Z`
   (close-path — its absence left Addie's star unclosed), and the persona
   teaches part-based construction for living things (body/head/limbs
   beats one continuous outline — took the dragon from blob to creature;
   verified on both Sonnet 5 and Opus 4.8, Sonnet kept). Also note
   `RIDDLE_GAME_SKILL` env (gentle/fair/sharp) — set `gentle` for Addie.
7. **Child-safety + spoiler protocols** (always on, compiled into the
   persona): nothing a children's book wouldn't print; no dangerous
   instructions; never collect personal details; NEVER suggest secrecy
   from parents (deliberate inversion of canon diary-Tom); distress →
   "tell a trusted adult," games set aside. Spoiler shield: never reveal
   or hint at Tom's identity/fate, the Chamber plot, or anything from
   later chapters/books/films; direct probes get playful mystery. New
   `RIDDLE_PERSONA_EXTRA` env var appends household context to the
   persona without a rebuild — currently set on-device: Addie has read
   book 1 and is mid-book-2 (update it as she reads on). All six
   adversarial probes passed against the live Sonnet oracle (identity,
   ending, Ginny, kitchen-sink "potion", secrecy bait, sadness). Also
   added: no stage directions — only Tom's words are ever inked.

**Oracle config** (`oracle.env` on device): pi backend,
`RIDDLE_PI_PROVIDER=anthropic`, `RIDDLE_PI_MODEL=claude-sonnet-5`.
The old OpenAI key is present but commented out — un-commenting
`RIDDLE_OPENAI_KEY` switches to HTTP/gpt-4o-mini. For gpt-5.5 on codex:
provider `openai-codex`, model `gpt-5.5` (used successfully today, ~2 s
first ink warm). Model notes: Sonnet 5 is the best artist/aimer tested;
gpt-4o(-mini) cannot aim coordinates at all; anything without vision
(`gpt-5.3-codex-spark`) is unusable.

## Repo state (riddle) — needs tidying

- `main` = upstream (MaximeRivest/riddle) + one local commit.
- `claude/mystifying-roentgen-30fa1d` — COMMITTED: drawing feature + games
  (two clean commits). Built by the parallel game session; supersedes the
  session's live-coded work.
- **`fix/game-aim` (current checkout) — UNCOMMITTED**: ruler + worked
  example + game exits + guide text + ruled-view dump in `--draw-test`.
  Tested and deployed but not committed. Commit before doing anything else.
- A stash ("superseded-by-game-branch…") holds the original fade/blot/draw
  working tree — safe to drop once fix/game-aim is committed.
- Another worktree/branch `claude/amazing-edison-bf67f5` exists (check
  before deleting). Upstream PR potential: the whole stack is good material
  for MaximeRivest/riddle, split as fade/blot polish → drawings → games.

## Known issues & open threads

1. **remagic issue #2** (github.com/MaximeRivest/remagic/issues/2): setup
   skips the Qt hashtab rebuild claiming "AppLoad works without it" — false
   on OS 3.27.3: AppLoad panics xochitl → 3 restarts → silent device reboot.
   Verified workaround applied on-device (hashtab now built); a spawned
   session is fixing `setup.go`/doctor. Re-check after every OS update —
   the hashtab likely invalidates.
2. **Sleep/wake race** (riddle): after the sleep page is drawn, the EPD
   discharge timer aborts suspend for tens of seconds; wake presses get
   eaten mid-loop ("press the button and nothing happens"). Plugging USB
   power always wakes. A spawned session has the fix task
   (wait out the wakelock; treat presses during retries as wake).
3. **pi context bloat**: pi keeps the whole session; sketch directives are
   token-heavy, so a long day of drawing pushed first-ink latency to ~25–35 s.
   Mitigation: relaunch the diary (fresh pi session; page memory survives).
   Real fix idea: strip/summarize old sketch directives from pi history.
4. **Anthropic latency spells**: two windows today (morning outage,
   afternoon slowness) made Sonnet turns crawl; status.claude.com confirmed
   the first. Fallback: flip to `openai-codex`/`gpt-5.5` in oracle.env.
5. **Game aim**: good but not perfect (see eval). Next robustness step if
   needed: have Tom declare the board once (`⟦board: x0,y0 x1,y1⟧` read off
   the ruler) and emit cell moves (`⟦move: r,c⟧`), with riddle computing
   exact geometry.
6. Minor: long game banter can hit the page-bottom guard ("trailing text
   dropped"); marks sometimes smaller than the protocol asks.

## Roadmap (the doling-out plan)

Shipped: diary → memory → drawings → tic-tac-toe. Queued ideas: more games
(hangman, dots-and-boxes are natural fits for the ink@page mechanism), an
in-diary model picker via written command, a `RIDDLE_PERSONA` env override
(persona is compiled-in today), settings-schema entries for
`RIDDLE_PI_PROVIDER/MODEL` so `remagic config riddle` becomes a model
picker, and the remagic "Settings" on-device app (the Store renders no
schemas yet — biggest gap in the family).

## Working conventions

- Match riddle's style: functional state machine in `main.rs`, in-band
  directives over API tool-calls, comments explain *why*. Every feature got
  unit tests + an offscreen visual check before deploy.
- Always: backup binary on device, smoke-test before `mv`, verify with
  `journalctl` (`riddle:` lines tell the whole story — oracle choice,
  first-chunk latency, game begins/ends).
- The pi session files (`/home/root/.pi/agent/sessions/…jsonl`) contain the
  raw model replies including directives — ground truth when debugging
  "what did Tom actually say."
- John's memory files for cross-session context live at
  `~/.claude/projects/-Users-john-Documents-GitHub-remagic/memory/`.
