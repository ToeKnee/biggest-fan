//! The CPU stressor: burn every core with the hottest instructions this
//! machine has. On this project's reference hardware (Zen 5, full AVX-512)
//! that means chaotic-map 512-bit FMA chains, lane-crossing shuffles and
//! ternary logic — the instructions that actually produce heat, not the
//! `nop`-adjacent filler a generic stressor burns through.
//!
//! The chains step `x = x*x + c` near the onset of chaos: there is no
//! closed form a compiler could collapse to, and the values never settle,
//! so input bits keep flipping. Every worker folds its register state
//! into a shared hash at exit, so nothing here is optimisable away.

use crate::thermal::{self, LabError};
use clap::{Args, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Iterations done before a worker re-checks the stop flag: long enough
/// that the check costs nothing measurable, short enough that a guard trip
/// or deadline lands within a millisecond.
const CHUNK: u32 = 2048;

#[derive(Args, Clone, Debug)]
pub struct CpuArgs {
    /// Seconds of stress to run.
    #[arg(long, default_value_t = 60)]
    pub secs: u64,
    /// Worker threads (default: one per logical core).
    #[arg(long)]
    pub threads: Option<usize>,
    /// Which instruction mix to burn.
    #[arg(long, value_enum, default_value_t = Method::Auto)]
    pub method: Method,
    /// k10temp (or GPU) temperature that aborts the run; 0 disables.
    #[arg(long, default_value_t = 90.0)]
    pub max_temp_c: f64,
    /// hwmon root to find k10temp under (test seam).
    #[arg(long, default_value = "/sys/class/hwmon")]
    pub hwmon_root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Method {
    /// FMA if this CPU has AVX-512, plain float chains otherwise.
    Auto,
    /// Chaotic-map 512-bit f64 FMA chains (AVX-512F).
    Fma,
    /// Lane-crossing shuffles + ternary logic + integer math (AVX-512F).
    Int,
    /// FMA chains alternating with integer shuffle/logic, so both the FP
    /// and integer ports stay busy.
    Mix,
}

impl Default for CpuArgs {
    fn default() -> Self {
        Self {
            secs: 60,
            threads: None,
            method: Method::Auto,
            max_temp_c: 90.0,
            hwmon_root: PathBuf::from("/sys/class/hwmon"),
        }
    }
}

/// The CLI wrapper: fresh flags, one printed summary line, and a thermal
/// trip becomes [`LabError::Thermal`] (exit code 3 upstream).
pub fn run(args: &CpuArgs) -> Result<i32, LabError> {
    let stop = Arc::new(AtomicBool::new(false));
    let tripped = Arc::new(AtomicBool::new(false));
    let report = stress(args, stop, tripped)?;
    println!("stress-lab cpu: {}", report.summary);
    if report.tripped {
        return Err(LabError::Thermal(format!(
            "cpu stress aborted by the thermal guard after {:.1} s",
            report.elapsed_secs
        )));
    }
    Ok(0)
}

/// Burns the CPU until `stop` goes up or `args.secs` elapses — the
/// library entry point the GUI's Stress lab panel uses. The caller owns
/// the flags: raising `stop` ends the run early, and `tripped` goes up
/// when the thermal guard aborts it. Nothing is printed; the numbers
/// arrive as a [`thermal::StressReport`].
pub fn stress(
    args: &CpuArgs,
    stop: Arc<AtomicBool>,
    tripped: Arc<AtomicBool>,
) -> Result<thermal::StressReport, LabError> {
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |count| count.get()));
    let avx512 = is_x86_feature_detected!("avx512f");
    let method = match args.method {
        Method::Auto if avx512 => Method::Fma,
        Method::Auto => Method::Mix, // Mix degrades to its scalar path
        Method::Fma | Method::Int if !avx512 => {
            return Err(LabError::Failed(format!(
                "{} needs AVX-512, which this CPU lacks",
                label(args.method)
            )));
        }
        other => other,
    };

    let iterations = Arc::new(AtomicU64::new(0));
    let state_hash = Arc::new(AtomicU64::new(0));

    let guard = thermal::spawn_guard(
        thermal::k10temp_path(&args.hwmon_root),
        args.max_temp_c,
        Arc::clone(&stop),
        Arc::clone(&tripped),
    );

    let start = Instant::now();
    let mut workers = Vec::with_capacity(threads);
    for index in 0..threads {
        let stop = Arc::clone(&stop);
        let iterations = Arc::clone(&iterations);
        let state_hash = Arc::clone(&state_hash);
        workers.push(
            std::thread::Builder::new()
                .name(format!("stress-cpu-{index}"))
                .spawn(move || match method {
                    #[cfg(target_arch = "x86_64")]
                    Method::Fma => unsafe { fma_worker(&stop, &iterations, &state_hash, index) },
                    #[cfg(target_arch = "x86_64")]
                    Method::Int => unsafe { int_worker(&stop, &iterations, &state_hash, index) },
                    #[cfg(target_arch = "x86_64")]
                    Method::Mix if is_x86_feature_detected!("avx512f") => unsafe {
                        mix_worker(&stop, &iterations, &state_hash, index)
                    },
                    _ => scalar_worker(&stop, &iterations, &state_hash, index),
                })
                .expect("spawn a stress worker"),
        );
    }

    // Home thread paces the deadline, polling `stop` so a thermal trip
    // ends the wait early.
    let deadline = start + Duration::from_secs(args.secs);
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
    if let Some(guard) = guard {
        let _ = guard.join();
    }

    let elapsed = start.elapsed().as_secs_f64().max(1e-6);
    let iterations_total = iterations.load(Ordering::Relaxed);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a throughput report does not need exact large-integer conversion"
    )]
    let per_second = iterations_total as f64 / elapsed;
    let mut summary = format!(
        "{threads} threads, method {}: {iterations_total} iterations in {elapsed:.1} s \
         ({:.2} Miter/s, state hash {:016x})",
        label(method),
        per_second / 1e6,
        state_hash.load(Ordering::Relaxed),
    );
    if method == Method::Fma {
        // 8 map-step FMAs per iteration, 2 flops each, 8 f64 lanes.
        #[expect(clippy::cast_precision_loss, reason = "throughput reporting only")]
        let gflops = iterations_total as f64 * 128.0 / elapsed / 1e9;
        summary.push_str(&format!(", ~{gflops:.0} f64 GFLOP/s across the package"));
    }
    Ok(thermal::StressReport {
        elapsed_secs: elapsed,
        summary,
        tripped: tripped.load(Ordering::Relaxed),
    })
}

const fn label(method: Method) -> &'static str {
    match method {
        Method::Auto => "auto",
        Method::Fma => "fma (AVX-512)",
        Method::Int => "int (AVX-512)",
        Method::Mix => "mix (AVX-512 or scalar)",
    }
}

/// Fold eight zmm registers into one u64 so the loops stay observable (and
/// unoptimisable) from outside.
#[cfg(target_arch = "x86_64")]
unsafe fn fold_hash(state: &[std::arch::x86_64::__m512d; 8], hash: &AtomicU64) {
    use std::arch::x86_64::*;
    // Safety: caller detected AVX-512F.
    let folded = unsafe {
        let ab = _mm512_xor_pd(state[0], state[1]);
        let cd = _mm512_xor_pd(state[2], state[3]);
        let ef = _mm512_xor_pd(state[4], state[5]);
        let gh = _mm512_xor_pd(state[6], state[7]);
        let mid = _mm512_xor_pd(_mm512_xor_pd(ab, cd), _mm512_xor_pd(ef, gh));
        std::mem::transmute::<__m512d, [f64; 8]>(mid)
    };
    let bits = folded.iter().enumerate().fold(0u64, |acc, (lane, value)| {
        // Rotate each lane by a different amount before XORing: workers
        // start from `set1` registers (identical lanes), and XORing an
        // even count of identical values cancels to zero — which would
        // hide whether the loops ran at all.
        acc ^ value
            .to_bits()
            .rotate_left(u32::try_from(lane * 7).expect("lane rotation fits in u32"))
    });
    hash.fetch_xor(bits, Ordering::Relaxed);
}

/// Eight chaotic heater chains, one per zmm register: each step is
/// `x = x*x + c` with `c` just above −2, the onset of chaos for this
/// quadratic map (Chebyshev-adjacent). The orbits stay bounded inside
/// roughly [−2, 2] yet never settle to a fixed point, so the FMA input
/// bits keep flipping (heat) and the final XOR fold stays seed-
/// sensitive (a converged chain state would cancel to zero — see the
/// worker tests). Eight chains keep every FMA unit busy.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn fma_worker(
    stop: &AtomicBool,
    iterations: &AtomicU64,
    hash: &AtomicU64,
    thread_index: usize,
) {
    use std::arch::x86_64::*;
    // Per-thread jitter keeps worker hashes distinct; c stays inside the
    // chaotic band (−2.005 .. −1.99) for any plausible thread count.
    let jitter = (thread_index as f64) * 0.000_976_562_5;
    let (k0, k1, k2, k3, k4, k5, k6, k7) = (
        _mm512_set1_pd(jitter - 2.0),
        _mm512_set1_pd(jitter - 1.99),
        _mm512_set1_pd(jitter - 1.995),
        _mm512_set1_pd(jitter - 1.998),
        _mm512_set1_pd(jitter - 2.005),
        _mm512_set1_pd(jitter - 1.992),
        _mm512_set1_pd(jitter - 2.002),
        _mm512_set1_pd(jitter - 1.997),
    );
    let lo = _mm512_set1_pd(-2.5);
    let hi = _mm512_set1_pd(2.5);
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
        _mm512_set1_pd(-1.9 + jitter),
        _mm512_set1_pd(1.7 + jitter),
        _mm512_set1_pd(-1.3 + jitter),
        _mm512_set1_pd(1.1 + jitter),
        _mm512_set1_pd(-0.9 + jitter),
        _mm512_set1_pd(1.5 + jitter),
        _mm512_set1_pd(-1.6 + jitter),
        _mm512_set1_pd(1.9 + jitter),
    );
    loop {
        for _ in 0..CHUNK {
            a = _mm512_fmadd_pd(a, a, k0);
            b = _mm512_fmadd_pd(b, b, k1);
            c = _mm512_fmadd_pd(c, c, k2);
            d = _mm512_fmadd_pd(d, d, k3);
            e = _mm512_fmadd_pd(e, e, k4);
            f = _mm512_fmadd_pd(f, f, k5);
            g = _mm512_fmadd_pd(g, g, k6);
            h = _mm512_fmadd_pd(h, h, k7);
            for value in [
                &mut a, &mut b, &mut c, &mut d, &mut e, &mut f, &mut g, &mut h,
            ] {
                *value = _mm512_min_pd(_mm512_max_pd(*value, lo), hi);
            }
        }
        iterations.fetch_add(u64::from(CHUNK), Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    unsafe { fold_hash(&[a, b, c, d, e, f, g, h], hash) };
}

/// Lane-crossing shuffles (`vpermt2q`) feeding ternary logic and integer
/// add/sub/xor/shift: exercises the ports FMA burns no cycles on. The
/// selector index rotates so the shuffle pattern changes every iteration.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn int_worker(
    stop: &AtomicBool,
    iterations: &AtomicU64,
    hash: &AtomicU64,
    thread_index: usize,
) {
    use std::arch::x86_64::*;
    // SplitMix64's golden-ratio constant, used as a per-thread bit mixer.
    let seed = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(thread_index as u64 + 1);
    let mut a = _mm512_set1_epi64(seed as i64);
    let mut b = _mm512_set1_epi64(seed.rotate_left(17) as i64 ^ -1);
    let mut c = _mm512_set1_epi64(seed.rotate_left(31) as i64);
    let mut d = _mm512_set1_epi64(seed.rotate_left(47) as i64);
    let mut e = _mm512_set1_epi64(seed.rotate_left(11) as i64);
    let mut f = _mm512_set1_epi64(seed.rotate_left(23) as i64);
    let mut selector = _mm512_set1_epi64(seed as i64 & 7);
    let one = _mm512_set1_epi64(1);
    let mask7 = _mm512_set1_epi64(7);
    loop {
        for _ in 0..CHUNK {
            // Per-lane shuffle across both source vectors (0..15 range).
            let permuted = _mm512_permutex2var_epi64(a, selector, b);
            // 0x96 is the XOR3 truth table: permuted ^ c ^ b.
            let mixed = _mm512_ternarylogic_epi64(permuted, c, b, 0x96);
            let a_next = _mm512_add_epi64(mixed, c);
            let b_next = _mm512_sub_epi64(a, mixed);
            let c_next = _mm512_xor_si512(mixed, _mm512_slli_epi64(permuted, 13));
            // Extra running registers: rotate/add/sub chains give the hash
            // fold distinct values to mix (and keep more ALUs busy).
            let d_next = _mm512_xor_si512(_mm512_rol_epi64(c, 13), b);
            let e_next = _mm512_add_epi64(mixed, selector);
            let f_next = _mm512_sub_epi64(c, permuted);
            selector = _mm512_and_si512(_mm512_add_epi64(selector, one), mask7);
            a = a_next;
            b = b_next;
            c = c_next;
            d = d_next;
            e = e_next;
            f = f_next;
        }
        iterations.fetch_add(u64::from(CHUNK), Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    // Reinterpret the integer state as floats purely to reuse fold_hash's
    // bit folding; only the bits matter.
    let state = unsafe {
        [
            std::mem::transmute::<__m512i, __m512d>(a),
            std::mem::transmute::<__m512i, __m512d>(b),
            std::mem::transmute::<__m512i, __m512d>(c),
            std::mem::transmute::<__m512i, __m512d>(d),
            std::mem::transmute::<__m512i, __m512d>(e),
            std::mem::transmute::<__m512i, __m512d>(f),
            std::mem::transmute::<__m512i, __m512d>(selector),
            std::mem::transmute::<__m512i, __m512d>(one),
        ]
    };
    unsafe { fold_hash(&state, hash) };
}

/// Alternate chaotic map FMAs with shuffle/ternary rounds so the FP and
/// integer pipes both stay back-to-back busy; the shuffle bits feed one
/// FMA chain so the two sides are genuinely coupled.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn mix_worker(
    stop: &AtomicBool,
    iterations: &AtomicU64,
    hash: &AtomicU64,
    thread_index: usize,
) {
    use std::arch::x86_64::*;
    let jitter = (thread_index as f64) * 0.000_976_562_5;
    // Chaotic-band map constants, same reasoning as `fma_worker`.
    let (ka, kb, ke, kf) = (
        _mm512_set1_pd(jitter - 2.0),
        _mm512_set1_pd(jitter - 1.99),
        _mm512_set1_pd(jitter - 1.995),
        _mm512_set1_pd(jitter - 2.005),
    );
    let lo = _mm512_set1_pd(-2.5);
    let hi = _mm512_set1_pd(2.5);
    let mut a = _mm512_set1_pd(1.4 + jitter);
    let mut b = _mm512_set1_pd(0.8 - jitter);
    let mut c = _mm512_set1_pd(1.2 + jitter);
    let mut d = _mm512_set1_pd(-0.9 + jitter);
    let mut e = _mm512_set1_pd(0.5 + jitter);
    let mut f = _mm512_set1_pd(-1.8 + jitter);
    let one = _mm512_set1_epi64(1);
    let mask7 = _mm512_set1_epi64(7);
    let mut selector = _mm512_set1_epi64(3);
    loop {
        for _ in 0..CHUNK {
            a = _mm512_fmadd_pd(a, a, ka);
            b = _mm512_fmadd_pd(b, b, kb);
            let permuted = unsafe {
                _mm512_permutex2var_epi64(
                    std::mem::transmute::<__m512d, __m512i>(a),
                    selector,
                    std::mem::transmute::<__m512d, __m512i>(b),
                )
            };
            let mixed = unsafe {
                _mm512_ternarylogic_epi64(
                    permuted,
                    std::mem::transmute::<__m512d, __m512i>(c),
                    std::mem::transmute::<__m512d, __m512i>(d),
                    0x96,
                )
            };
            e = _mm512_fmadd_pd(e, e, ke);
            f = unsafe { _mm512_fmadd_pd(mixed_float(mixed), f, kf) };
            selector = _mm512_and_si512(_mm512_add_epi64(selector, one), mask7);
            for value in [&mut a, &mut b, &mut c, &mut d, &mut e, &mut f] {
                *value = _mm512_min_pd(_mm512_max_pd(*value, lo), hi);
            }
        }
        iterations.fetch_add(u64::from(CHUNK), Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    let state = unsafe {
        [
            a,
            b,
            c,
            d,
            e,
            f,
            std::mem::transmute::<__m512i, __m512d>(selector),
            std::mem::transmute::<__m512i, __m512d>(mask7),
        ]
    };
    unsafe { fold_hash(&state, hash) };
}

/// View an integer vector's bits as floats; mix only ever *bit*-folds it.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn mixed_float(value: std::arch::x86_64::__m512i) -> std::arch::x86_64::__m512d {
    unsafe { std::mem::transmute(value) }
}

/// No AVX-512 (or a non-x86 build): the same chaotic map chains in plain
/// scalar f64. Weak, but a fallback keeps the tool honest everywhere.
fn scalar_worker(stop: &AtomicBool, iterations: &AtomicU64, hash: &AtomicU64, thread_index: usize) {
    let jitter = (thread_index as f64) * 0.001;
    // Explicit element type: without it the literals stay ambiguous
    // `{float}` and `mul_add`/`clamp`/`to_bits` cannot resolve.
    let mut state: [f64; 8] = [1.1, -0.9, 1.3, 0.7, -1.7, 0.6, 1.9, -0.8];
    // Per-chain constants from the chaotic band (see `fma_worker`).
    let constants = [
        jitter - 2.0,
        jitter - 1.99,
        jitter - 1.995,
        jitter - 1.998,
        jitter - 2.005,
        jitter - 1.992,
        jitter - 2.002,
        jitter - 1.997,
    ];
    loop {
        for _ in 0..CHUNK {
            for (value, constant) in state.iter_mut().zip(constants) {
                *value = value.mul_add(*value, constant);
            }
            for value in &mut state {
                *value = value.clamp(-2.5, 2.5);
            }
        }
        iterations.fetch_add(u64::from(CHUNK), Ordering::Relaxed);
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }
    let bits = state.iter().fold(0u64, |acc, value| acc ^ value.to_bits());
    hash.fetch_xor(bits, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workers must actually burn and count: run one chunk's worth by
    /// letting the stop flag trip immediately, and expect progress plus a
    /// state hash change (proof the loops are not optimised away).
    #[test]
    fn workers_count_iterations_and_fold_state() {
        let stop = AtomicBool::new(true);
        let iterations = AtomicU64::new(0);
        let hash = AtomicU64::new(0);
        scalar_worker(&stop, &iterations, &hash, 1);
        assert_eq!(iterations.load(Ordering::Relaxed), u64::from(CHUNK));
        assert_ne!(hash.load(Ordering::Relaxed), 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx512_workers_run_when_the_cpu_has_avx512() {
        if !is_x86_feature_detected!("avx512f") {
            return; // Not this machine; nothing to check.
        }
        for worker in [fma_worker, int_worker, mix_worker] {
            let stop = AtomicBool::new(true);
            let iterations = AtomicU64::new(0);
            let hash = AtomicU64::new(0);
            unsafe { worker(&stop, &iterations, &hash, 2) };
            assert_eq!(iterations.load(Ordering::Relaxed), u64::from(CHUNK));
            assert_ne!(hash.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn method_resolution_never_offers_missing_isa() {
        // The rules `run` applies: explicit AVX-512 methods must be
        // rejected without the ISA, and Auto always lands somewhere that
        // exists. Checked here through the same decision as run uses.
        let avx512 = is_x86_feature_detected!("avx512f");
        let resolved = |method: Method| match method {
            Method::Auto if avx512 => Some(Method::Fma),
            Method::Auto => Some(Method::Mix),
            Method::Fma | Method::Int if !avx512 => None,
            other => Some(other),
        };
        assert_eq!(
            resolved(Method::Auto),
            Some(if avx512 { Method::Fma } else { Method::Mix })
        );
        assert_eq!(resolved(Method::Int).is_some(), avx512);
    }

    /// Same seed, same final state: two single-chunk runs of `mix_worker`
    /// must fold identical hash bits, proving the AVX-512 recurrences are
    /// deterministic (and not, say, reading uninitialised lanes).
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn mix_worker_state_is_deterministic() {
        if !is_x86_feature_detected!("avx512f") {
            return; // Not this machine; nothing to check.
        }
        let mut hashes = [0u64; 2];
        for slot in &mut hashes {
            let stop = AtomicBool::new(true);
            let iterations = AtomicU64::new(0);
            let hash = AtomicU64::new(0);
            unsafe { mix_worker(&stop, &iterations, &hash, 0) };
            *slot = hash.load(Ordering::Relaxed);
        }
        assert_eq!(hashes[0], hashes[1], "same seed must land the same state");
        assert_ne!(hashes[0], 0, "the final state must be folded in");
    }

    /// The GUI raises the stop flag from outside; a running `stress` must
    /// wrap up cleanly with a report instead of waiting out the deadline.
    #[test]
    fn stress_finishes_cleanly_when_the_caller_raises_stop() {
        let args = CpuArgs {
            secs: 30,
            threads: Some(1),
            max_temp_c: 0.0, // guard off: a busy test machine must not trip it
            ..CpuArgs::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            stress(&args, stop_for_thread, Arc::new(AtomicBool::new(false)))
        });
        std::thread::sleep(Duration::from_millis(400));
        stop.store(true, Ordering::Relaxed);
        let outcome = worker.join().expect("the stress thread must not panic");
        let report = outcome.expect("one worker thread and no AVX-512 gate on the scalar path");
        assert!(
            !report.tripped,
            "with the guard off the trip flag stays down"
        );
        assert!(
            report.elapsed_secs < 20.0,
            "raising stop cuts the deadline short"
        );
        assert!(
            !report.summary.is_empty(),
            "the report carries a summary line"
        );
    }
}
