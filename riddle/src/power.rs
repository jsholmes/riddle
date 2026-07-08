//! Power button, for takeover mode. The device is GRABBED so logind doesn't
//! also act on the press: the diary draws its sleep page first, then triggers
//! the suspend itself. If the grab fails we still see the press and draw, and
//! leave the actual suspend to logind.

use std::io;
use std::os::fd::RawFd;

const EV_KEY: u16 = 1;
const KEY_POWER: u16 = 116;
const EVIOCGRAB: libc::c_ulong = 0x40044590;

pub struct PowerButton {
    fd: RawFd,
    pub grabbed: bool,
}

impl PowerButton {
    pub fn open() -> io::Result<Self> {
        for i in 0..8 {
            let name = std::fs::read_to_string(format!("/sys/class/input/event{i}/device/name"))
                .unwrap_or_default()
                .to_lowercase();
            if !name.contains("powerkey") && !name.contains("power button") {
                continue;
            }
            let cpath = std::ffi::CString::new(format!("/dev/input/event{i}")).unwrap();
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let grabbed = unsafe { libc::ioctl(fd, EVIOCGRAB, 1i32) } == 0;
            eprintln!("riddle: power button /dev/input/event{i} (grabbed: {grabbed})");
            return Ok(Self { fd, grabbed });
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "no power button device"))
    }

    /// True if a power-key press (value 1) was seen since the last drain.
    pub fn drain_pressed(&mut self) -> bool {
        let mut pressed = false;
        let mut buf = [0u8; 24 * 16];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            for chunk in buf[..n as usize].chunks_exact(24) {
                let etype = u16::from_le_bytes(chunk[16..18].try_into().unwrap());
                let code = u16::from_le_bytes(chunk[18..20].try_into().unwrap());
                let value = i32::from_le_bytes(chunk[20..24].try_into().unwrap());
                if etype == EV_KEY && code == KEY_POWER && value == 1 {
                    pressed = true;
                }
            }
        }
        pressed
    }
}

impl Drop for PowerButton {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, EVIOCGRAB, 0i32);
            libc::close(self.fd);
        }
    }
}

fn read_sysfs_u64(path: impl AsRef<std::path::Path>) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The kernel's successful-suspend counter — the authoritative "we slept"
/// signal. (Clock heuristics fail here: on this kernel CLOCK_MONOTONIC keeps
/// advancing across deep sleep, verified on-device.)
pub fn suspend_count() -> u64 {
    read_sysfs_u64("/sys/power/suspend_stats/success").unwrap_or(0)
}

/// True while any kernel wakeup source is active. The EPD regulator holds one
/// for up to ~30s after ANY panel update — including the just-drawn sleep
/// page — and a suspend attempted inside that window aborts ("Some devices
/// failed to suspend"). `active_time_ms` is nonzero exactly while a source is
/// active, so this is a cheap "will suspend stick yet?" probe.
fn wakeup_source_active() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/wakeup") else {
        return false;
    };
    dir.flatten()
        .any(|e| read_sysfs_u64(e.path().join("active_time_ms")).is_some_and(|ms| ms > 0))
}

#[derive(Clone, Copy, PartialEq)]
pub enum SleepOutcome {
    /// The device suspended and resumed (or suspend never stuck and we gave up).
    Woke,
    /// A button press arrived while still awake: never mind, stay up.
    Cancelled,
}

/// Suspend (confirmed via the kernel's success counter) and block until wake.
/// The sleep page just drawn arms the EPD discharge timer, so wait for wakeup
/// sources to clear before each attempt instead of hammering suspend, and
/// keep reading the button: while we're still awake a press is a cancel, not
/// input for the suspended device to lose.
pub fn suspend_until_wake(btn: &mut PowerButton) -> SleepOutcome {
    use std::time::{Duration, Instant};
    let count0 = suspend_count();
    let mut attempts = 0;
    let outcome = 'sleeping: loop {
        // Outwait active wakeup sources: the ≤30s EPD timer before the first
        // attempt, only short-lived stragglers before retries.
        let budget = Duration::from_secs(if attempts == 0 { 35 } else { 5 });
        let hold = Instant::now();
        loop {
            // Count first: in the ungrabbed fallback logind may suspend us
            // any time, making the next press a wake, not a cancel.
            if suspend_count() > count0 {
                break 'sleeping SleepOutcome::Woke;
            }
            if btn.drain_pressed() {
                break 'sleeping SleepOutcome::Cancelled;
            }
            if !wakeup_source_active() || hold.elapsed() >= budget {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if btn.grabbed {
            let _ = std::process::Command::new("systemctl").arg("suspend").status();
        }
        attempts += 1;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(6) {
            std::thread::sleep(Duration::from_millis(400));
            if suspend_count() > count0 {
                break 'sleeping SleepOutcome::Woke;
            }
            if btn.drain_pressed() {
                break 'sleeping SleepOutcome::Cancelled;
            }
        }
        if attempts >= 8 {
            eprintln!("riddle: suspend never happened ({attempts} tries); waking the page");
            return SleepOutcome::Woke;
        }
        if attempts == 1 {
            eprintln!("riddle: suspend aborted (wakeup source held), retrying quietly");
        }
    };
    match outcome {
        SleepOutcome::Woke => eprintln!("riddle: waking (suspend attempts: {attempts})"),
        SleepOutcome::Cancelled => eprintln!("riddle: sleep cancelled (power button)"),
    }
    outcome
}

/// After resume, Wi-Fi is often stranded: wpa_supplicant fails a few attempts
/// while the radio settles and marks the network TEMP-DISABLED, and with
/// xochitl stopped nobody clears it. Nudge it back, detached, best-effort.
pub fn wifi_heal() {
    let script = "for i in 1 2 3 4 5 6 7 8 9 10; do \
        state=$(wpa_cli -i wlan0 status 2>/dev/null | grep ^wpa_state | cut -d= -f2); \
        [ \"$state\" = COMPLETED ] && exit 0; \
        wpa_cli -i wlan0 enable_network all >/dev/null 2>&1; \
        wpa_cli -i wlan0 reassociate >/dev/null 2>&1; \
        sleep 3; \
        done";
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
