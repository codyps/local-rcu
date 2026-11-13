//! The measurements this crate's design decisions have been made on, in one harness. CI records
//! every row per commit of main (`bench-history.yml`) and comments on each pull request with
//! what it changes (`bench.yml`).
//!
//! Run with: `cargo bench --bench isolated [-- FILTER...]` (a row is produced if its name
//! contains any `FILTER`).
//!
//! Each row is `<group>/<what>  <value> <unit>`. The groups:
//!
//! - `instructions`: instructions per operation, single threaded, counted by running this
//!   binary under `valgrind --tool=callgrind` (rows are skipped when valgrind is missing).
//!   Exact and repeatable, so even a 1% change on a shared CI runner is real.
//! - `allocations`: allocator calls per operation (including the caller's `Box`), for the same
//!   scenarios. Exact.
//! - `memory`, `held back`: bytes a slot and its readers take, and the old values an idle
//!   pinned `CachedReader` keeps from being collected. Exact.
//! - `ns per read`, `ns per write`: wall time with real reader and writer threads all going
//!   flat out, for `R` readers. Best of `BENCH_PASSES` passes. These depend on the machine and
//!   on what else runs on it: a configuration is only measured when every thread gets a CPU
//!   of its own, and on shared runners only large changes mean anything.
//!
//! Environment: `BENCH_PASSES` (default 3), `BENCH_MS` (measured time per pass, default 400),
//! `BENCH_MAX_READERS` (default: what the CPU count allows), `BENCH_NO_VALGRIND` (skip the
//! `instructions` rows).

use local_rcu::cached::CachedReader;
use local_rcu::{Reader, Writer};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::io::Write as _;
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Big enough that it is not stored inline anywhere, small enough that making one is cheap.
type Val = [u64; 4];

fn val(i: u64) -> Val {
    [i; 4]
}

// ---------------------------------------------------------------------------------------------
// counting allocator

/// Counts allocator calls and live bytes of the calling thread. Thread local, so the threaded
/// benchmarks pay an increment and not a shared cache line.
struct Counting;

struct Counts {
    allocs: Cell<u64>,
    live_bytes: Cell<i64>,
}

thread_local! {
    static COUNTS: Counts = const { Counts { allocs: Cell::new(0), live_bytes: Cell::new(0) } };
}

fn count(allocs: u64, bytes: i64) {
    // `try_with`: the allocator is also called while a thread's locals are torn down
    let _ = COUNTS.try_with(|c| {
        c.allocs.set(c.allocs.get() + allocs);
        c.live_bytes.set(c.live_bytes.get() + bytes);
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count(1, l.size() as i64);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        count(0, -(l.size() as i64));
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        count(1, new_size as i64 - l.size() as i64);
        System.realloc(p, l, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocs() -> u64 {
    COUNTS.with(|c| c.allocs.get())
}

fn live_bytes() -> i64 {
    COUNTS.with(|c| c.live_bytes.get())
}

// ---------------------------------------------------------------------------------------------
// output

struct Out {
    filters: Vec<String>,
}

impl Out {
    fn wants(&self, name: &str) -> bool {
        self.filters.is_empty() || self.filters.iter().any(|f| name.contains(f.as_str()))
    }

    fn row(&self, name: &str, value: f64, unit: &str) {
        if !self.wants(name) {
            return;
        }
        // the name ends at the first run of two spaces: see `bench_parse.py`
        debug_assert!(!name.contains("  "));
        let value = match unit {
            "instr" | "bytes" => format!("{value:.1}"),
            "values" => format!("{value:.0}"),
            _ => format!("{value:.2}"),
        };
        println!("{name:<72}  {value:>10} {unit}");
        let _ = std::io::stdout().flush();
    }
}

fn env_num(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number")),
        Err(_) => default,
    }
}

// ---------------------------------------------------------------------------------------------
// single threaded scenarios: instructions and allocations per operation

/// Operations run before counting starts, so that pools and `Vec`s have their steady size.
const WARM: u64 = 512;

struct Runner {
    n: u64,
    allocs: u64,
}

impl Runner {
    #[inline(always)]
    fn run(&mut self, mut op: impl FnMut(u64)) {
        for i in 0..WARM {
            op(i);
        }
        let before = allocs();
        for i in WARM..WARM + self.n {
            op(i);
        }
        self.allocs = allocs() - before;
    }
}

const SCENARIOS: &[&str] = &[
    "read(), plain reader",
    "read(), CachedReader, value unchanged",
    "write(), no readers",
    "write(), 1 idle reader",
    "write(), 6 idle readers",
    "write(), 1 reader reads after each write",
    "write(), 1 guard held across each write",
    "write(), 6 guards held across each write",
    "write(), 1 guard held throughout",
    "write(), 1 idle pinned CachedReader",
    "write(), 1 CachedReader reads after each write",
    "write_sync_into(), no readers",
    "write_sync_into(), 1 reader reads after each write",
    "write_sync_into() reusing boxes, no readers",
    "write_sync_into() reusing boxes, 1 reader reads after each write",
    "write_nosync() + try_sync() every 128, 1 idle reader",
];

/// How often the batching writer looks for old values.
const BATCH: u64 = 128;

// the `Box`es are the point: they are what the writer hands back
#[allow(clippy::vec_box)]
fn reuse(old: &mut Vec<Box<Val>>, i: u64) -> Box<Val> {
    match old.pop() {
        Some(mut b) => {
            *b = val(i);
            b
        }
        None => Box::new(val(i)),
    }
}

/// Run scenario `id` (an index into [`SCENARIOS`]). One operation is everything the name says:
/// "reads after each write" is a write and a read.
fn scenario(id: usize, r: &mut Runner) {
    let mut w = Writer::new(Box::new(val(0)));
    let mut old: Vec<Box<Val>> = Vec::new();
    match id {
        0 => {
            let mut rd = w.reader();
            r.run(|_| {
                black_box(rd.read()[0]);
            });
        }
        1 => {
            let mut rd = CachedReader::new(w.reader());
            r.run(|_| {
                black_box(rd.read()[0]);
            });
        }
        2 => r.run(|i| drop(w.write(Box::new(val(i))))),
        3 => {
            let mut rd = w.reader();
            black_box(rd.read()[0]);
            r.run(|i| drop(w.write(Box::new(val(i)))));
        }
        4 => {
            let mut rs: [Reader<Val>; 6] = std::array::from_fn(|_| w.reader());
            for rd in &mut rs {
                black_box(rd.read()[0]);
            }
            r.run(|i| drop(w.write(Box::new(val(i)))));
        }
        5 => {
            let mut rd = w.reader();
            r.run(|i| {
                drop(w.write(Box::new(val(i))));
                black_box(rd.read()[0]);
            });
        }
        6 => {
            let mut rd = w.reader();
            r.run(|i| {
                let g = rd.read();
                drop(w.write(Box::new(val(i))));
                drop(g);
            });
        }
        7 => {
            let mut rs: [Reader<Val>; 6] = std::array::from_fn(|_| w.reader());
            r.run(|i| {
                let [a, b, c, d, e, f] = &mut rs;
                let gs = (a.read(), b.read(), c.read(), d.read(), e.read(), f.read());
                drop(w.write(Box::new(val(i))));
                drop(gs);
            });
        }
        8 => {
            let mut rd = w.reader();
            let g = rd.read();
            r.run(|i| drop(w.write(Box::new(val(i)))));
            drop(g);
        }
        9 => {
            let mut rd = CachedReader::new(w.reader());
            black_box(rd.read()[0]);
            r.run(|i| drop(w.write(Box::new(val(i)))));
        }
        10 => {
            let mut rd = CachedReader::new(w.reader());
            r.run(|i| {
                drop(w.write(Box::new(val(i))));
                black_box(rd.read()[0]);
            });
        }
        11 => r.run(|i| {
            w.write_sync_into(Box::new(val(i)), &mut old);
            old.clear();
        }),
        12 => {
            let mut rd = w.reader();
            r.run(|i| {
                w.write_sync_into(Box::new(val(i)), &mut old);
                old.clear();
                black_box(rd.read()[0]);
            });
        }
        13 => r.run(|i| {
            let v = reuse(&mut old, i);
            w.write_sync_into(v, &mut old);
        }),
        14 => {
            let mut rd = w.reader();
            r.run(|i| {
                let v = reuse(&mut old, i);
                w.write_sync_into(v, &mut old);
                black_box(rd.read()[0]);
            });
        }
        15 => {
            let mut rd = w.reader();
            black_box(rd.read()[0]);
            r.run(|i| {
                w.write_nosync(Box::new(val(i)));
                if i % BATCH == BATCH - 1 {
                    drop(w.try_sync());
                }
            });
        }
        _ => panic!("no scenario {id}"),
    }
}

/// Instructions the whole process executes for scenario `id` with `n` counted operations.
fn instructions_total(exe: &std::path::Path, id: usize, n: u64) -> Result<u64, String> {
    let out = Command::new("valgrind")
        .args(["--tool=callgrind", "--callgrind-out-file=/dev/null"])
        .arg(exe)
        .args(["--instr-child", &id.to_string(), &n.to_string()])
        .output()
        .map_err(|e| format!("cannot run valgrind: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!("valgrind failed: {}", stderr.trim()));
    }
    // "==1234== Collected : 5678"
    stderr
        .lines()
        .find_map(|l| l.split_once("Collected :"))
        .and_then(|(_, n)| n.split_whitespace().next()?.parse().ok())
        .ok_or_else(|| {
            format!(
                "no instruction count in valgrind's output: {}",
                stderr.trim()
            )
        })
}

/// Instructions per operation of scenario `id`: the difference between two runs that differ
/// only in the number of operations, which cancels process start up and the scenario's setup.
fn instructions_per_op(exe: &std::path::Path, id: usize) -> Result<f64, String> {
    const FEW: u64 = 1_000;
    const MANY: u64 = 11_000;
    let few = instructions_total(exe, id, FEW)?;
    let many = instructions_total(exe, id, MANY)?;
    Ok(many.saturating_sub(few) as f64 / (MANY - FEW) as f64)
}

fn single_threaded(out: &Out) {
    let ids: Vec<usize> = (0..SCENARIOS.len()).collect();

    for &id in &ids {
        let name = format!("allocations/{}", SCENARIOS[id]);
        if out.wants(&name) {
            let mut r = Runner {
                n: 10_000,
                allocs: 0,
            };
            scenario(id, &mut r);
            out.row(&name, r.allocs as f64 / r.n as f64, "allocs");
        }
    }

    let wanted: Vec<usize> = ids
        .into_iter()
        .filter(|&id| out.wants(&format!("instructions/{}", SCENARIOS[id])))
        .collect();
    if wanted.is_empty() {
        return;
    }
    if std::env::var_os("BENCH_NO_VALGRIND").is_some() {
        eprintln!("# instructions: skipped (BENCH_NO_VALGRIND)");
        return;
    }
    let exe = std::env::current_exe().expect("own path");
    // the children are single threaded and independent: run them side by side
    let workers = thread::available_parallelism().map_or(1, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    thread::scope(|s| {
        for _ in 0..workers.min(wanted.len()) {
            s.spawn(|| {
                while let Some(&id) = wanted.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let r = instructions_per_op(&exe, id);
                    results.lock().unwrap().push((id, r));
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|(id, _)| *id);
    let mut complained = false;
    for (id, r) in results {
        match r {
            Ok(n) => out.row(&format!("instructions/{}", SCENARIOS[id]), n, "instr"),
            Err(e) if !complained => {
                complained = true;
                eprintln!("# instructions: skipped ({e})");
            }
            Err(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// memory and held back values

fn memory(out: &Out) {
    const READERS: usize = 1024;
    // not part of what is measured
    let mut rs: Vec<Reader<Val>> = Vec::with_capacity(READERS);

    let b0 = live_bytes();
    let w = Writer::new(Box::new(val(0)));
    let b1 = live_bytes();
    rs.extend((0..READERS).map(|_| w.reader()));
    let b2 = live_bytes();

    out.row(
        "memory/slot with no readers (32 byte value)",
        (b1 - b0) as f64,
        "bytes",
    );
    out.row(
        "memory/each reader (of 1024)",
        (b2 - b1) as f64 / READERS as f64,
        "bytes",
    );
}

fn held_back(out: &Out) {
    const WRITES: u64 = 100_000;
    let name = "held back/1 idle pinned CachedReader, after 100000 writes";
    if !out.wants(name) {
        return;
    }
    let mut w = Writer::new(Box::new(val(0)));
    let mut rd = CachedReader::new(w.reader());
    black_box(rd.read()[0]);
    let mut old = Vec::new();
    for i in 0..WRITES {
        w.write_sync_into(Box::new(val(i)), &mut old);
    }
    // every write replaced one value
    out.row(name, (WRITES - old.len() as u64) as f64, "values");
}

/// `write()` while `guards` readers sit in a read section, so that every value is held back
/// and the writer's list of them grows.
fn guards_held(out: &Out, passes: u64) {
    const WRITES: u64 = 2_000;
    for guards in [1usize, 2, 6] {
        let name = format!(
            "ns per write/write(), {guards} guard{} held throughout ({WRITES} values held back)",
            if guards == 1 { "" } else { "s" }
        );
        if !out.wants(&name) {
            continue;
        }
        let mut best = f64::MAX;
        for _ in 0..passes.max(5) {
            let mut w = Writer::new(Box::new(val(0)));
            let mut rs: Vec<_> = (0..guards).map(|_| w.reader()).collect();
            let gs: Vec<_> = rs.iter_mut().map(|r| r.read()).collect();
            let t = Instant::now();
            for i in 0..WRITES {
                drop(w.write(Box::new(val(i))));
            }
            best = best.min(t.elapsed().as_nanos() as f64 / WRITES as f64);
            drop(gs);
        }
        out.row(&name, best, "ns");
    }
}

// ---------------------------------------------------------------------------------------------
// threaded: wall time per read and per write

/// On a line of its own: it is loaded by every thread, all the time, and must not share a line
/// with something that is written (a reader's epoch, say).
#[repr(align(128))]
struct Phase(AtomicU8);

const WARMING: u8 = 0;
const MEASURING: u8 = 1;
const DONE: u8 = 2;

/// Run `op` until the phase is `DONE`. Returns the number of operations done while measuring,
/// and how long this thread took for them.
#[inline(always)]
fn timed(phase: &Phase, batch: u64, mut op: impl FnMut()) -> (u64, Duration) {
    while phase.0.load(Ordering::Relaxed) == WARMING {
        for _ in 0..batch {
            op();
        }
    }
    let t = Instant::now();
    let mut n = 0;
    while phase.0.load(Ordering::Relaxed) == MEASURING {
        for _ in 0..batch {
            op();
        }
        n += batch;
    }
    (n, t.elapsed())
}

#[derive(Clone, Copy, PartialEq)]
enum ReaderKind {
    Plain,
    Cached,
}

impl ReaderKind {
    fn name(self) -> &'static str {
        match self {
            ReaderKind::Plain => "plain",
            ReaderKind::Cached => "cached",
        }
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    Write,
    Into,
    Reuse,
    Batched,
}

impl WriteMode {
    const ALL: [WriteMode; 4] = [
        WriteMode::Write,
        WriteMode::Into,
        WriteMode::Reuse,
        WriteMode::Batched,
    ];

    fn name(self) -> &'static str {
        match self {
            WriteMode::Write => "write()",
            WriteMode::Into => "write_sync_into()",
            WriteMode::Reuse => "write_sync_into() reusing boxes",
            WriteMode::Batched => "write_nosync() + try_sync() every 128",
        }
    }
}

struct Job {
    kind: ReaderKind,
    readers: usize,
    writer: Option<WriteMode>,
    read_row: Option<String>,
    write_row: Option<String>,
}

/// One pass: `(ns per read, ns per write)`, each `NAN` when there was nothing of the kind.
fn threaded_pass(job: &Job, measure: Duration) -> (f64, f64) {
    let mut w = Writer::new(Box::new(val(0)));
    let phase = Arc::new(Phase(AtomicU8::new(WARMING)));

    let reader_threads: Vec<_> = (0..job.readers)
        .map(|_| {
            let rd = w.reader();
            let phase = phase.clone();
            let kind = job.kind;
            thread::spawn(move || match kind {
                ReaderKind::Plain => {
                    let mut rd = rd;
                    timed(&phase, 256, || {
                        black_box(rd.read()[0]);
                    })
                }
                ReaderKind::Cached => {
                    let mut rd = CachedReader::new(rd);
                    timed(&phase, 256, || {
                        black_box(rd.read()[0]);
                    })
                }
            })
        })
        .collect();

    // without a writer thread the writer still has to outlive the readers
    let (idle_writer, writer_thread) = match job.writer {
        None => (Some(w), None),
        Some(mode) => {
            let phase = phase.clone();
            let t = thread::spawn(move || {
                let mut i = 0u64;
                let mut old: Vec<Box<Val>> = Vec::new();
                match mode {
                    WriteMode::Write => timed(&phase, 32, || {
                        i += 1;
                        drop(w.write(Box::new(val(i))));
                    }),
                    WriteMode::Into => timed(&phase, 32, || {
                        i += 1;
                        w.write_sync_into(Box::new(val(i)), &mut old);
                        old.clear();
                    }),
                    WriteMode::Reuse => timed(&phase, 32, || {
                        i += 1;
                        let v = reuse(&mut old, i);
                        w.write_sync_into(v, &mut old);
                    }),
                    WriteMode::Batched => timed(&phase, 32, || {
                        i += 1;
                        w.write_nosync(Box::new(val(i)));
                        if i % BATCH == BATCH - 1 {
                            drop(w.try_sync());
                        }
                    }),
                }
            });
            (None, Some(t))
        }
    };

    thread::sleep(measure / 4);
    phase.0.store(MEASURING, Ordering::Relaxed);
    thread::sleep(measure);
    phase.0.store(DONE, Ordering::Relaxed);

    let (mut reads, mut read_time) = (0u64, Duration::ZERO);
    for t in reader_threads {
        let (n, d) = t.join().unwrap();
        reads += n;
        read_time += d;
    }
    let write = match writer_thread {
        Some(t) => {
            let (n, d) = t.join().unwrap();
            d.as_nanos() as f64 / n as f64
        }
        None => f64::NAN,
    };
    drop(idle_writer);
    (read_time.as_nanos() as f64 / reads as f64, write)
}

fn threaded(out: &Out, passes: u64, measure: Duration) {
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let max_readers = env_num("BENCH_MAX_READERS", cpus as u64) as usize;
    let counts: Vec<usize> = [1, 3, 6]
        .into_iter()
        .filter(|&r| r <= max_readers)
        .collect();

    let mut jobs = Vec::new();
    for mode in WriteMode::ALL {
        jobs.push(Job {
            kind: ReaderKind::Plain,
            readers: 0,
            writer: Some(mode),
            read_row: None,
            write_row: Some(format!("ns per write/{}, no readers", mode.name())),
        });
    }
    for &readers in &counts {
        for kind in [ReaderKind::Plain, ReaderKind::Cached] {
            jobs.push(Job {
                kind,
                readers,
                writer: None,
                read_row: Some(format!(
                    "ns per read/R={readers} {}, no writer",
                    kind.name()
                )),
                write_row: None,
            });
        }
        // the writer gets a CPU of its own
        if readers + 1 > max_readers.max(2) {
            continue;
        }
        for (kind, mode, read_row) in [
            (ReaderKind::Plain, WriteMode::Write, true),
            (ReaderKind::Cached, WriteMode::Write, true),
            (ReaderKind::Plain, WriteMode::Into, false),
            (ReaderKind::Plain, WriteMode::Reuse, false),
            (ReaderKind::Plain, WriteMode::Batched, true),
        ] {
            jobs.push(Job {
                kind,
                readers,
                writer: Some(mode),
                read_row: read_row.then(|| {
                    format!(
                        "ns per read/R={readers} {}, writer: {}",
                        kind.name(),
                        mode.name()
                    )
                }),
                write_row: Some(format!(
                    "ns per write/{}, R={readers} {} readers",
                    mode.name(),
                    kind.name()
                )),
            });
        }
    }

    for job in &jobs {
        let wanted = |row: &Option<String>| row.as_ref().is_some_and(|n| out.wants(n));
        if !wanted(&job.read_row) && !wanted(&job.write_row) {
            continue;
        }
        let (mut read, mut write) = (f64::MAX, f64::MAX);
        for _ in 0..passes {
            let (r, w) = threaded_pass(job, measure);
            read = read.min(r);
            write = write.min(w);
        }
        if let Some(name) = &job.read_row {
            out.row(name, read, "ns");
        }
        if let Some(name) = &job.write_row {
            out.row(name, write, "ns");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // a child of `instructions_total()`: do the work and nothing else
    if args.first().is_some_and(|a| a == "--instr-child") {
        let id = args[1].parse().expect("scenario id");
        let n = args[2].parse().expect("operation count");
        scenario(id, &mut Runner { n, allocs: 0 });
        return;
    }

    let out = Out {
        // cargo passes `--bench`
        filters: args.into_iter().filter(|a| !a.starts_with("--")).collect(),
    };
    let passes = env_num("BENCH_PASSES", 3).max(1);
    let measure = Duration::from_millis(env_num("BENCH_MS", 400));
    eprintln!(
        "# {} cpus, best of {passes} passes of {measure:?}",
        thread::available_parallelism().map_or(1, |n| n.get())
    );

    single_threaded(&out);
    memory(&out);
    held_back(&out);
    guards_held(&out, passes);
    threaded(&out, passes, measure);
}
