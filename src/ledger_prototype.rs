//! PROTOTYPE, throwaway: effect ledger supervisor wiring.
//!
//! Answers "What do the supervisor and its child say to each other?" for the
//! effect-ledger map. Hand-written, no codegen, payloads are opaque bytes.
//! Unix only. Not production code: errors mostly abort with a message.
//!
//! Switch: `ROC_LEDGER=record:<dir>` or `ROC_LEDGER=replay:<dir>`, and
//! (MVP 3) `ROC_LEDGER=dump:<dir>`, which prints what the request did.
//! A ledger is a directory: `<dir>/spans.jsonl` (OTLP JSON Lines) and
//! `<dir>/exe` (the pinned binary).

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Value};

use crate::ledger_codec;

const SPANS: &str = "spans.jsonl";
const PINNED: &str = "exe";
const CHILD_ENV: &str = "ROC_LEDGER_CHILD";

// The boundary the pinned binary knows. Codegen would emit all 85 symbols.
const BOUNDARY: &[&str] = &[
    "hosted_stdout_line",
    "hosted_stderr_line",
    "hosted_stdin_line",
    // MVP
    "hosted_file_read_utf8",
    "hosted_utc_now",
];

// ---------------------------------------------------------------- frames
//
// One frame = kind:u8, len:u32 LE, body. Child -> supervisor:
const F_BEGIN: u8 = 1; //    seq:u64, symbol_len:u16, symbol, request bytes
const F_END: u8 = 2; //      seq:u64, response bytes
const F_RETURNED: u8 = 3; // roc_main's return value: i32
const F_CRASHED: u8 = 4; //  roc_crashed message
const F_EXPECT: u8 = 5; //   roc_expect_failed message
const F_PANIC: u8 = 6; //    Rust panic message (a host or ledger bug)
// Supervisor -> child (replay only):
const F_RESPOND: u8 = 101; // die_after:i32 (0 = none), response bytes
const F_DIE: u8 = 102; //     signal:i32, redelivered at the crossing boundary

fn write_frame(w: &mut impl Write, kind: u8, body: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(5 + body.len());
    buf.push(kind);
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
    w.write_all(&buf)
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some((head[0], body)))
}

fn begin_body(seq: u64, symbol: &str, request: &[u8]) -> Vec<u8> {
    let mut b = seq.to_le_bytes().to_vec();
    b.extend_from_slice(&(symbol.len() as u16).to_le_bytes());
    b.extend_from_slice(symbol.as_bytes());
    b.extend_from_slice(request);
    b
}

fn parse_begin(b: &[u8]) -> (u64, String, Vec<u8>) {
    let seq = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let n = u16::from_le_bytes(b[8..10].try_into().unwrap()) as usize;
    let symbol = String::from_utf8_lossy(&b[10..10 + n]).into_owned();
    (seq, symbol, b[10 + n..].to_vec())
}

// ---------------------------------------------------------------- entry

/// Called first thing in `rust_main`. Returns `Some(code)` when this process
/// is a supervisor (it never runs Roc code), `None` when it should run the app,
/// either plainly or as a ledger child (then `link()` is set).
pub fn start(process_args: &[OsString]) -> Option<i32> {
    if let Some(spec) = std::env::var_os(CHILD_ENV) {
        // Strip before any Roc code runs, so the app sees the env it would
        // see without a ledger.
        std::env::remove_var(CHILD_ENV);
        become_child(&spec.to_string_lossy());
        return None;
    }
    let spec = std::env::var_os("ROC_LEDGER")?.to_string_lossy().into_owned();
    let usage = "ROC_LEDGER must be record:<dir>, replay:<dir> or dump:<dir>";
    let (mode, dir) = spec.split_once(':').unwrap_or_else(|| die(64, usage));
    let dir = PathBuf::from(dir);
    match mode {
        "record" => Some(record(&dir, process_args)),
        "replay" => Some(replay(&dir, process_args)),
        "dump" => Some(dump(&dir, process_args)),
        _ => die(64, usage),
    }
}

fn die(code: i32, msg: &str) -> ! {
    eprintln!("[ledger] {msg}");
    std::process::exit(code)
}

// ---------------------------------------------------------------- child side

pub struct Link {
    replaying: bool,
    to_sup: Mutex<File>,
    from_sup: Option<Mutex<File>>,
    seq: AtomicU64,
}

static LINK: OnceLock<Link> = OnceLock::new();

pub fn link() -> Option<&'static Link> {
    LINK.get()
}

fn become_child(spec: &str) {
    // "record:<wfd>" or "replay:<wfd>:<rfd>"
    let parts: Vec<&str> = spec.split(':').collect();
    let fd = |s: &str| -> File {
        let fd: i32 = s.parse().unwrap();
        // Back to close-on-exec, so a grandchild spawned by Cmd never holds
        // the pipe open (the supervisor would then never see EOF).
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        unsafe { File::from_raw_fd(fd) }
    };
    let new_link = match parts.as_slice() {
        ["record", w] => Link { replaying: false, to_sup: Mutex::new(fd(w)), from_sup: None, seq: AtomicU64::new(0) },
        ["replay", w, r] => {
            // rr keeps one process group so job control works; SIGINT/SIGQUIT
            // are blocked here so Ctrl-C reaches only the supervisor. (Also
            // blocked in pre_exec; this is belt and braces.)
            block_signals(&[libc::SIGINT, libc::SIGQUIT]);
            Link { replaying: true, to_sup: Mutex::new(fd(w)), from_sup: Some(Mutex::new(fd(r))), seq: AtomicU64::new(0) }
        }
        _ => die(64, "bad ROC_LEDGER_CHILD"),
    };
    let _ = LINK.set(new_link);
    std::panic::set_hook(Box::new(|info| {
        if let Some(l) = link() {
            l.send(F_PANIC, info.to_string().as_bytes());
        }
        eprintln!("{info}");
    }));
}

fn block_signals(sigs: &[i32]) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in sigs {
            libc::sigaddset(&mut set, *s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn die_by_signal(sig: i32) -> ! {
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, sig);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
        libc::_exit(128 + sig)
    }
}

impl Link {
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    fn send(&self, kind: u8, body: &[u8]) {
        let mut w = self.to_sup.lock().unwrap_or_else(|p| p.into_inner());
        if write_frame(&mut *w, kind, body).is_err() {
            // The supervisor is gone; nothing may happen unrecorded.
            unsafe { libc::_exit(1) }
        }
    }

    /// Before the inner call. Carries the request, so the effect in flight at
    /// a death is known, and so replay can compare before answering.
    pub fn begin(&self, symbol: &str, request: &[u8]) -> u64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(F_BEGIN, &begin_body(seq, symbol, request));
        seq
    }

    /// Record: after the inner call. Carries only the response.
    pub fn end(&self, seq: u64, response: &[u8]) {
        let mut b = seq.to_le_bytes().to_vec();
        b.extend_from_slice(response);
        self.send(F_END, &b);
    }

    /// Replay: block until the supervisor answers this crossing.
    pub fn await_response(&self) -> Vec<u8> {
        let mut r = self.from_sup.as_ref().unwrap().lock().unwrap();
        match read_frame(&mut *r) {
            Ok(Some((F_RESPOND, b))) => {
                let die_after = i32::from_le_bytes(b[0..4].try_into().unwrap());
                if die_after != 0 {
                    // Killed between crossings in the recording: deliver as
                    // soon as the last recorded crossing returns.
                    die_by_signal(die_after);
                }
                b[4..].to_vec()
            }
            Ok(Some((F_DIE, b))) => die_by_signal(i32::from_le_bytes(b[0..4].try_into().unwrap())),
            // Supervisor gone or halted us at a divergence.
            _ => unsafe { libc::_exit(1) },
        }
    }
}

pub fn returned(code: i32) {
    if let Some(l) = link() {
        l.send(F_RETURNED, &code.to_le_bytes());
    }
}

pub fn crashed(msg: &[u8]) {
    if let Some(l) = link() {
        l.send(F_CRASHED, msg);
    }
}

pub fn expect_failed(msg: &[u8]) {
    if let Some(l) = link() {
        l.send(F_EXPECT, msg);
    }
}

// ---------------------------------------------------------------- outcome

#[derive(Debug, Clone, PartialEq)]
struct InFlight {
    seq: u64,
    effect: String,
    request: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Returned(i32),
    Crashed(String),
    Signaled { signal: i32, in_flight: Option<InFlight> },
    /// Exited without roc_main returning (e.g. an exit inside host code).
    Exited(i32),
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

impl Outcome {
    fn to_json(&self) -> Value {
        match self {
            Outcome::Returned(c) => json!({"returned": c}),
            Outcome::Crashed(m) => json!({"crashed": m}),
            Outcome::Exited(c) => json!({"exited": c}),
            Outcome::Signaled { signal, in_flight } => json!({
                "signal": signal,
                "in_flight": in_flight.as_ref().map(|f| json!({"seq": f.seq, "effect": f.effect, "request": b64(&f.request)})),
            }),
        }
    }

    fn from_json(bytes: &[u8]) -> Option<Outcome> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        if let Some(c) = v.get("returned") {
            return Some(Outcome::Returned(c.as_i64()? as i32));
        }
        if let Some(m) = v.get("crashed") {
            return Some(Outcome::Crashed(m.as_str()?.to_string()));
        }
        if let Some(c) = v.get("exited") {
            return Some(Outcome::Exited(c.as_i64()? as i32));
        }
        let in_flight = match v.get("in_flight").filter(|f| !f.is_null()) {
            None => None,
            Some(f) => Some(InFlight {
                seq: f["seq"].as_u64()?,
                effect: f["effect"].as_str()?.to_string(),
                request: unb64(f["request"].as_str()?)?,
            }),
        };
        Some(Outcome::Signaled { signal: v["signal"].as_i64()? as i32, in_flight })
    }

    fn describe(&self) -> String {
        match self {
            Outcome::Returned(c) => format!("returned {c}"),
            Outcome::Crashed(m) => format!("crashed: {m}"),
            Outcome::Exited(c) => format!("exited {c} without returning"),
            Outcome::Signaled { signal, in_flight: Some(f) } => {
                format!("killed by {} during seq {} ({})", sig_name(*signal), f.seq, f.effect)
            }
            Outcome::Signaled { signal, in_flight: None } => {
                format!("killed by {} between crossings", sig_name(*signal))
            }
        }
    }
}

fn sig_name(sig: i32) -> String {
    match sig {
        libc::SIGINT => "SIGINT".into(),
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGQUIT => "SIGQUIT".into(),
        libc::SIGHUP => "SIGHUP".into(),
        n => format!("signal {n}"),
    }
}

/// Collects one request's frames on the supervisor side.
#[derive(Default)]
struct ChildEnd {
    returned: Option<i32>,
    crashed: Option<String>,
    panicked: Option<String>,
}

impl ChildEnd {
    fn absorb(&mut self, kind: u8, body: &[u8]) -> bool {
        match kind {
            F_RETURNED => self.returned = Some(i32::from_le_bytes(body[0..4].try_into().unwrap())),
            F_CRASHED => self.crashed = Some(String::from_utf8_lossy(body).into_owned()),
            F_PANIC => self.panicked = Some(String::from_utf8_lossy(body).into_owned()),
            F_EXPECT => eprintln!("[ledger] expect failed: {}", String::from_utf8_lossy(body)),
            _ => return false,
        }
        true
    }

    fn outcome(&self, status: std::process::ExitStatus, in_flight: Option<InFlight>) -> Outcome {
        if let Some(sig) = status.signal() {
            return Outcome::Signaled { signal: sig, in_flight };
        }
        if let Some(m) = &self.crashed {
            return Outcome::Crashed(m.clone());
        }
        match self.returned {
            Some(c) => Outcome::Returned(c),
            None => Outcome::Exited(status.code().unwrap_or(-1)),
        }
    }
}

// ---------------------------------------------------------------- OTLP lines

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Trace {
    trace_id: String,
    root_span_id: String,
    resource: Value,
}

fn attr_str(k: &str, v: &str) -> Value {
    json!({"key": k, "value": {"stringValue": v}})
}

fn attr_int(k: &str, v: i64) -> Value {
    json!({"key": k, "value": {"intValue": v.to_string()}})
}

impl Trace {
    /// Each span is its own `TracesData` line, resource included.
    fn line(&self, span: Value) -> String {
        let v = json!({"resourceSpans": [{
            "resource": self.resource,
            "scopeSpans": [{"scope": {"name": "roc.effect_ledger"}, "spans": [span]}],
        }]});
        let mut s = v.to_string();
        s.push('\n');
        s
    }

    fn crossing(&self, seq: u64, effect: &str, start: u128, end: u128, request: &[u8], response: &[u8]) -> String {
        self.line(json!({
            "traceId": self.trace_id,
            "spanId": random_hex(8),
            "parentSpanId": self.root_span_id,
            "name": effect,
            "kind": 3, // CLIENT: core asks the world
            "startTimeUnixNano": start.to_string(),
            "endTimeUnixNano": end.to_string(),
            "attributes": [
                attr_int("ledger.seq", seq as i64),
                attr_str("ledger.effect", effect),
                attr_str("ledger.request", &b64(request)),
                attr_str("ledger.response", &b64(response)),
            ],
        }))
    }

    fn root(&self, start: u128, end: u128, args: &[OsString], outcome: &Outcome) -> String {
        let ok = matches!(outcome, Outcome::Returned(0));
        self.line(json!({
            "traceId": self.trace_id,
            "spanId": self.root_span_id,
            "name": "main!",
            "kind": 2, // SERVER: the host asks the core
            "startTimeUnixNano": start.to_string(),
            "endTimeUnixNano": end.to_string(),
            "attributes": [
                attr_int("ledger.seq", 0),
                attr_str("ledger.effect", "main!"),
                attr_str("ledger.request", &b64(&encode_args(args))),
                attr_str("ledger.response", &b64(outcome.to_json().to_string().as_bytes())),
            ],
            // The outcome also lands in standard OTLP status, so a backend
            // shows a crashed or killed request without decoding payloads.
            "status": if ok { json!({"code": 1}) } else { json!({"code": 2, "message": outcome.describe()}) },
        }))
    }
}

fn encode_args(args: &[OsString]) -> Vec<u8> {
    let mut b = Vec::new();
    for a in args {
        b.extend_from_slice(&(a.len() as u32).to_le_bytes());
        b.extend_from_slice(a.as_bytes());
    }
    b
}

fn decode_args(mut b: &[u8]) -> Option<Vec<OsString>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = u32::from_le_bytes(b.get(0..4)?.try_into().unwrap()) as usize;
        out.push(OsString::from_vec(b.get(4..4 + n)?.to_vec()));
        b = &b[4 + n..];
    }
    Some(out)
}

// ---------------------------------------------------------------- pinning

/// clone, then hardlink, then copy (rr's order).
fn pin(src: &Path, dst: &Path) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        let s = std::ffi::CString::new(src.as_os_str().as_bytes()).unwrap();
        let d = std::ffi::CString::new(dst.as_os_str().as_bytes()).unwrap();
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
            return "clone";
        }
    }
    #[cfg(target_os = "linux")]
    {
        const FICLONE: libc::c_ulong = 0x40049409;
        if let (Ok(from), Ok(to)) = (File::open(src), File::create(dst)) {
            if unsafe { libc::ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) } == 0 {
                let _ = fs::set_permissions(dst, fs::metadata(src).unwrap().permissions());
                return "clone";
            }
            let _ = fs::remove_file(dst);
        }
    }
    if fs::hard_link(src, dst).is_ok() {
        return "hardlink";
    }
    fs::copy(src, dst).unwrap_or_else(|e| die(74, &format!("cannot pin binary: {e}")));
    "copy"
}

// MVP: pin each build once into `<ledgers>/.pins/`, and hardlink every
// ledger's exe to that copy. macOS charges ~280 ms the first time any new file
// is executed, so a fresh clone per request made recording ~50x slower. This
// pays it once per build. The shared copy is a clone (or copy) of the original,
// never a hardlink to it, so overwriting the original in place can't reach it.
fn pin_shared(src: &Path, dst: &Path) -> &'static str {
    use std::os::unix::fs::MetadataExt;
    let Some(store) = dst.parent().and_then(Path::parent).map(|ledgers| ledgers.join(".pins")) else {
        return pin(src, dst);
    };
    // The same file with the same contents: device, inode, size, mtime (ns).
    let m = fs::metadata(src).unwrap();
    let shared = store.join(format!("{:x}-{:x}-{:x}-{:x}.{:x}", m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec()));
    if !shared.exists() {
        let _ = fs::create_dir_all(&store);
        let tmp = store.join(format!(".tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        pin(src, &tmp);
        // A concurrent recorder may win the rename; both copies are the same build.
        let _ = fs::rename(&tmp, &shared);
    }
    if fs::hard_link(&shared, dst).is_ok() {
        return "shared";
    }
    pin(src, dst)
}

fn current_exe() -> PathBuf {
    #[cfg(target_os = "linux")]
    return PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe().unwrap()
}

// ---------------------------------------------------------------- signals

static CHILD_PID: AtomicI32 = AtomicI32::new(0);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_sigterm(_: i32) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
            libc::alarm(5);
        }
    }
}

extern "C" fn on_sigalrm(_: i32) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

extern "C" fn on_replay_sigint(_: i32) {
    INTERRUPTED.store(1, Ordering::SeqCst);
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn handle(sig: i32, f: extern "C" fn(i32)) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = f as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// Exit the way the child did, so the shell sees the same thing.
fn exit_like(status: std::process::ExitStatus) -> i32 {
    if let Some(sig) = status.signal() {
        die_by_signal(sig);
    }
    status.code().unwrap_or(1)
}

fn pipe() -> (File, File) {
    let mut fds = [0i32; 2];
    unsafe { libc::pipe(fds.as_mut_ptr()) };
    for fd in fds {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn child_command(exe: &Path, args: &[OsString], env_spec: String, keep_fds: Vec<i32>, block: Vec<i32>) -> Command {
    let mut cmd = Command::new(exe);
    if let Some(argv0) = args.first() {
        cmd.arg0(argv0); // keep the original argv0
    }
    cmd.args(&args[1..]).env_remove("ROC_LEDGER").env(CHILD_ENV, env_spec);
    unsafe {
        cmd.pre_exec(move || {
            for fd in &keep_fds {
                libc::fcntl(*fd, libc::F_SETFD, 0);
            }
            // The supervisor ignores these; ignored dispositions survive
            // exec, so put the defaults back for the app.
            for s in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGTERM, libc::SIGALRM] {
                libc::signal(s, libc::SIG_DFL);
            }
            block_signals(&block);
            Ok(())
        });
    }
    cmd
}

// ---------------------------------------------------------------- record

fn record(dir: &Path, process_args: &[OsString]) -> i32 {
    if dir.exists() {
        die(73, &format!("{} already exists", dir.display()));
    }
    fs::create_dir_all(dir).unwrap();
    let start = now_nanos();

    // Pin first, then run the child from the pinned copy.
    let exe = dir.join(PINNED);
    let how = pin_shared(&current_exe(), &exe);
    let mut spans = File::create(dir.join(SPANS)).unwrap();

    let service = process_args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let trace = Trace {
        // request_id IS the traceId: one request, one trace.
        trace_id: random_hex(16),
        // Chosen up front so children can name their parent before the root
        // exists; the root is written last.
        root_span_id: random_hex(8),
        resource: json!({"attributes": [
            attr_str("service.name", &service),
            attr_str("ledger.pinned", how),
            attr_str("ledger.basic_cli", "0.23.0"),
        ]}),
    };

    let (mut from_child, to_sup) = pipe();
    let wfd = to_sup.as_raw_fd();
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    handle(libc::SIGTERM, on_sigterm);
    handle(libc::SIGALRM, on_sigalrm);

    let mut child = child_command(&exe, process_args, format!("record:{wfd}"), vec![wfd], vec![])
        .spawn()
        .unwrap_or_else(|e| die(71, &format!("cannot start the pinned binary: {e}")));
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    drop(to_sup);

    let mut end = ChildEnd::default();
    let mut open: Option<(InFlight, u128)> = None;
    loop {
        let frame = match read_frame(&mut from_child) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => die(74, &format!("pipe: {e}")),
        };
        match frame {
            (F_BEGIN, b) => {
                let (seq, effect, request) = parse_begin(&b);
                open = Some((InFlight { seq, effect, request }, now_nanos()));
            }
            (F_END, b) => {
                let seq = u64::from_le_bytes(b[0..8].try_into().unwrap());
                let (f, t0) = open.take().expect("END without BEGIN");
                assert_eq!(f.seq, seq, "END for the wrong crossing");
                // Whole lines only: a line is written once its crossing ends.
                spans.write_all(trace.crossing(seq, &f.effect, t0, now_nanos(), &f.request, &b[8..]).as_bytes()).unwrap();
            }
            (k, b) => {
                end.absorb(k, &b);
            }
        }
    }
    let status = child.wait().unwrap();
    let outcome = end.outcome(status, open.map(|(f, _)| f));
    spans.write_all(trace.root(start, now_nanos(), process_args, &outcome).as_bytes()).unwrap();
    drop(spans);
    eprintln!("[ledger] recorded {} ({}) -> {}", outcome.describe(), how, dir.display());
    exit_like(status)
}

// ---------------------------------------------------------------- replay

struct Entry {
    seq: u64,
    effect: String,
    request: Vec<u8>,
    response: Vec<u8>,
}

impl Entry {
    fn from_span(span: &Value) -> Option<Entry> {
        let s = |k| attr(span, k).and_then(|v| v["stringValue"].as_str()).unwrap_or_default();
        Some(Entry {
            seq: attr(span, "ledger.seq")?["intValue"].as_str()?.parse().ok()?,
            effect: s("ledger.effect").to_string(),
            request: unb64(s("ledger.request"))?,
            response: unb64(s("ledger.response"))?,
        })
    }
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"].as_array()?.iter().find(|a| a["key"] == key).map(|a| &a["value"])
}

fn refuse(msg: &str) -> ! {
    die(65, &format!("refused: {msg}"))
}

fn load(dir: &Path) -> (Vec<Entry>, Vec<OsString>, Outcome) {
    let text = fs::read_to_string(dir.join(SPANS)).unwrap_or_else(|_| refuse("no spans.jsonl"));
    let mut entries = Vec::new();
    let mut root = None;
    for (n, line) in text.lines().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap_or_else(|_| refuse(&format!("torn line {}", n + 1)));
        let not_span = || format!("line {}: not a ledger span", n + 1);
        let Some(e) = Entry::from_span(&v["resourceSpans"][0]["scopeSpans"][0]["spans"][0]) else { refuse(&not_span()) };
        if e.seq == 0 {
            let (Some(args), Some(outcome)) = (decode_args(&e.request), Outcome::from_json(&e.response)) else { refuse(&not_span()) };
            root = Some((args, outcome));
        } else {
            if !BOUNDARY.contains(&e.effect.as_str()) {
                refuse(&format!("seq {} names {}, which is outside the boundary", e.seq, e.effect));
            }
            entries.push(e);
        }
    }
    let (args, outcome) = root.unwrap_or_else(|| refuse("incomplete: no main! span"));
    entries.sort_by_key(|e| e.seq);
    for (i, e) in entries.iter().enumerate() {
        if e.seq != i as u64 + 1 {
            refuse(&format!("gap or duplicate at seq {}", e.seq));
        }
    }
    (entries, args, outcome)
}

fn replay(dir: &Path, process_args: &[OsString]) -> i32 {
    let exe = dir.join(PINNED);
    if !exe.exists() {
        refuse("incomplete: no pinned binary");
    }
    // Supervisor and child must be one build: always exec the pinned binary.
    become_pinned(&exe, process_args);

    let (entries, args, recorded) = load(dir);

    let (mut from_child, to_sup) = pipe();
    let (from_sup, mut to_child) = pipe();
    let (wfd, rfd) = (to_sup.as_raw_fd(), from_sup.as_raw_fd());
    unsafe {
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    handle(libc::SIGINT, on_replay_sigint);

    // The recorded args, not this command line's.
    let mut child = child_command(&exe, &args, format!("replay:{wfd}:{rfd}"), vec![wfd, rfd], vec![libc::SIGINT, libc::SIGQUIT])
        .stdin(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| die(71, &format!("cannot start the pinned binary: {e}")));
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    drop((to_sup, from_sup));

    let mark = std::env::var_os("ROC_LEDGER_MARK").is_some(); // stands in for -M
    let mut end = ChildEnd::default();
    let mut matched: u64 = 0;
    let mut sent_die: Option<InFlight> = None;
    let mut divergence: Option<String> = None;

    let kill_child = |child: &std::process::Child| unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    };

    loop {
        let frame = match read_frame(&mut from_child) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        match frame {
            (F_BEGIN, b) => {
                let (seq, effect, request) = parse_begin(&b);
                let in_flight = match &recorded {
                    Outcome::Signaled { in_flight: Some(f), .. } if f.seq == seq => Some(f.clone()),
                    _ => None,
                };
                if let Some(f) = in_flight {
                    if f.effect != effect || f.request != request {
                        divergence = Some(format!("Mismatch at seq {seq} ({effect})"));
                        kill_child(&child);
                        break;
                    }
                    let Outcome::Signaled { signal, .. } = recorded else { unreachable!() };
                    let _ = write_frame(&mut to_child, F_DIE, &signal.to_le_bytes());
                    sent_die = Some(f);
                    continue;
                }
                let Some(e) = entries.get(seq as usize - 1) else {
                    divergence = Some(format!("Extra at seq {seq} ({effect})"));
                    kill_child(&child);
                    break;
                };
                if e.effect != effect || e.request != request {
                    divergence = Some(format!(
                        "Mismatch at seq {seq} ({effect})\n[ledger]   recorded: {} {:?}\n[ledger]   replayed: {} {:?}",
                        e.effect,
                        String::from_utf8_lossy(&e.request),
                        effect,
                        String::from_utf8_lossy(&request)
                    ));
                    kill_child(&child);
                    break;
                }
                // Test hook only: slow replay down so Ctrl-C can land mid-replay.
                if let Some(ms) = std::env::var("ROC_LEDGER_SLOW_MS").ok().and_then(|v| v.parse().ok()) {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                echo(&effect, &request, seq, mark);
                // Killed between crossings: the kill rides on the last answer.
                let die_after = match &recorded {
                    Outcome::Signaled { signal, in_flight: None } if seq as usize == entries.len() => *signal,
                    _ => 0,
                };
                let mut body = die_after.to_le_bytes().to_vec();
                body.extend_from_slice(&e.response);
                let _ = write_frame(&mut to_child, F_RESPOND, &body);
                matched = seq;
            }
            (k, b) => {
                end.absorb(k, &b);
            }
        }
    }
    let status = child.wait().unwrap();

    if INTERRUPTED.load(Ordering::SeqCst) == 1 {
        eprintln!("[ledger] interrupted after seq {matched}");
        return 130;
    }
    if let Some(p) = &end.panicked {
        eprintln!("[ledger] replay bug: Rust panic in the child: {p}");
        return 70;
    }
    if divergence.is_none() && (matched as usize) < entries.len() && sent_die.is_none() {
        let e = &entries[matched as usize];
        divergence = Some(format!("Missing at seq {} ({})", e.seq, e.effect));
    }
    let replayed = end.outcome(status, sent_die);
    if divergence.is_none() && replayed != recorded {
        divergence = Some(format!(
            "Mismatch at seq 0 (main!)\n[ledger]   recorded: {}\n[ledger]   replayed: {}",
            recorded.describe(),
            replayed.describe()
        ));
    }
    match divergence {
        None => {
            eprintln!("[ledger] reproduced: {} after {} crossings", replayed.describe(), entries.len());
            0
        }
        Some(d) => {
            eprintln!("[ledger] divergence: {d}");
            eprintln!("[ledger]   ledger: {}", dir.display());
            eprintln!("[ledger] This is a bug in the effect ledger: the pinned build replayed its own recording differently.");
            70
        }
    }
}

fn become_pinned(exe: &Path, process_args: &[OsString]) {
    if !same_file::is_same_file(current_exe(), exe).unwrap_or(false) {
        let mut cmd = Command::new(exe);
        if let Some(argv0) = process_args.first() {
            cmd.arg0(argv0);
        }
        let err = cmd.args(&process_args[1..]).exec();
        die(71, &format!("cannot exec the pinned binary: {err}"));
    }
}

/// Replay emulates stdio writes in the supervisor, as rr does; the child
/// itself performs nothing.
fn echo(effect: &str, request: &[u8], seq: u64, mark: bool) {
    let tag = if mark { format!("[seq {seq}] ") } else { String::new() };
    match effect {
        "hosted_stdout_line" => {
            let mut o = io::stdout().lock();
            let _ = write!(o, "{tag}");
            let _ = o.write_all(request);
            let _ = o.write_all(b"\n");
            let _ = o.flush();
        }
        "hosted_stderr_line" => {
            let mut o = io::stderr().lock();
            let _ = write!(o, "{tag}");
            let _ = o.write_all(request);
            let _ = o.write_all(b"\n");
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- dump

// MVP 3: what a request did, from its ledger alone. The pinned binary decodes
// its own payloads, so the build that wrote a ledger is the build that reads
// it. Lenient where replay refuses: a torn line or unknown symbol is shown.
fn dump(dir: &Path, process_args: &[OsString]) -> i32 {
    let exe = dir.join(PINNED);
    if !exe.exists() {
        die(66, "no pinned binary, so nothing here can decode the payloads");
    }
    become_pinned(&exe, process_args);
    let text = fs::read_to_string(dir.join(SPANS)).unwrap_or_else(|_| die(66, "no spans.jsonl"));

    struct Line {
        seq: u64,
        effect: String,
        start: u128,
        end: u128,
        request: Vec<u8>,
        response: Vec<u8>,
    }
    let mut crossings = Vec::new();
    let mut root: Option<Line> = None;
    let (mut trace_id, mut resource) = (String::new(), Value::Null);
    let mut problems = Vec::new();
    for (n, text_line) in text.lines().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(text_line) else {
            problems.push(format!("line {}: torn", n + 1));
            continue;
        };
        resource = v["resourceSpans"][0]["resource"].clone();
        let span = &v["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        trace_id = span["traceId"].as_str().unwrap_or_default().to_string();
        let nanos = |k: &str| span[k].as_str().and_then(|t| t.parse().ok()).unwrap_or(0);
        let Some(Entry { seq, effect, request, response }) = Entry::from_span(span) else {
            problems.push(format!("line {}: not a ledger span", n + 1));
            continue;
        };
        let line = Line { seq, effect, start: nanos("startTimeUnixNano"), end: nanos("endTimeUnixNano"), request, response };
        if seq == 0 {
            root = Some(line);
        } else {
            crossings.push(line);
        }
    }
    crossings.sort_by_key(|c| c.seq);

    let resource_attr = |k| attr(&resource, k).and_then(|v| v["stringValue"].as_str()).unwrap_or("?").to_string();
    let base = root.as_ref().map(|r| r.start).or(crossings.first().map(|c| c.start)).unwrap_or(0);
    let offset = |t: u128| format!("+{:.2}ms", (t.saturating_sub(base)) as f64 / 1e6);
    let call = |effect: &str, request: &[u8]| {
        let shown = ledger_codec::display_request(effect, request).unwrap_or_else(|| format!("<{} bytes: {}>", request.len(), b64(request)));
        format!("{effect}({shown})")
    };

    println!("ledger   {}", dir.display());
    println!("request  {trace_id}");
    println!("service  {} (basic-cli {}, pinned {})", resource_attr("service.name"), resource_attr("ledger.basic_cli"), resource_attr("ledger.pinned"));
    if let Some(r) = &root {
        let args = decode_args(&r.request).map_or("unreadable".into(), |a| format!("{:?}", a.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>()));
        println!("args     {args}");
        println!("started  {}", ledger_codec::iso_utc(r.start));
    }
    println!();
    println!("{:>4}  {:>9}  crossing", "seq", "at");
    for c in &crossings {
        let shown = ledger_codec::display_response(&c.effect, &c.response).unwrap_or_else(|| format!("<{} bytes: {}>", c.response.len(), b64(&c.response)));
        println!("{:>4}  {:>9}  {} -> {shown}", c.seq, offset(c.start), call(&c.effect, &c.request));
    }
    match &root {
        Some(r) => {
            let outcome = Outcome::from_json(&r.response);
            if let Some(Outcome::Signaled { in_flight: Some(f), signal }) = &outcome {
                println!("{:>4}  {:>9}  {} -> no response: {}", f.seq, "", call(&f.effect, &f.request), sig_name(*signal));
            }
            let described = outcome.map_or("unreadable outcome".into(), |o| o.describe());
            println!("{:>4}  {:>9}  {described}", "end", offset(r.end));
        }
        None => println!("{:>4}  {:>9}  no main! span: the request never finished, or its supervisor was killed", "end", ""),
    }
    for p in &problems {
        println!("[ledger] {p}");
    }
    0
}
