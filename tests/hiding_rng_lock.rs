//! `spin::Mutex<R>` held across rayon parallel work in Plonky3's hiding types.
//!
//! Three call sites lock the shared RNG and keep the guard alive while rayon work runs:
//!
//!   bug3  `MerkleTreeHidingMmcs::commit`     merkle-tree/src/hiding_mmcs.rs
//!   bug4  `HidingFriPcs::get_quotient_ldes`  fri/src/hiding_pcs.rs
//!   bug5  `HidingFriPcs::commit`             fri/src/hiding_pcs.rs
//!
//! Mechanism. A rayon worker that waits for one of its own sub-tasks does not sleep. It runs
//! other pending tasks of the pool (work stealing). If this worker holds the spin lock, and the
//! task it picks up calls the same method on the same shared instance, the task spins on a lock
//! that its own thread holds. `spin::Mutex` is not reentrant and never yields, so the thread
//! spins forever. The other workers then block on the same lock, and the pool deadlocks.
//!
//! Both types are `Sync` (upstream asserts this in `hiding_mmcs_is_sync`), so calling them
//! from several rayon tasks is valid use. Upstream `p3-batch-stark` does exactly this: it calls
//! `pcs.get_quotient_ldes` inside `(0..n_instances).into_par_iter()` (batch-stark/src/prover.rs).
//!
//! The `*_concurrent_*` tests must complete. On upstream they hang, and the watchdog turns the
//! hang into a test failure. The deadlock depends on thread scheduling, so a single run on
//! upstream can pass by chance. The tests use rayon's global pool. They were run on a 32-core
//! machine. bug3 hangs reliably only with more workers than cores, so run-tests.sh runs it with
//! `RAYON_NUM_THREADS` set to twice the core count.
//!
//! The `*_roundtrip` tests check that the fix keeps commit, open and verify working.
//!
//! Environment variables (all optional). bug5 has its own defaults, see `params_bug5`:
//!   REPRO_TASKS       parallel calls per round      (default: 64)
//!   REPRO_ROUNDS      rounds                        (default: 20)
//!   REPRO_LOG_HEIGHT  log2 of the trace height      (default: 14)
//!   REPRO_WIDTH       trace width                   (default: 32)
//!   REPRO_STALL_SECS  hang = no call completed in this many seconds (default: 60)
//!
//! Run each test in its own process (see run-tests.sh): after a hang, spinning threads keep
//! running until the process exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use p3_baby_bear::{BabyBear, Poseidon2BabyBear};
use p3_challenger::{CanObserve, DuplexChallenger, FieldChallenger};
use p3_commit::{
    ExtensionMmcs, Mmcs, OpeningRequest, Pcs, PolynomialSpace, UnivariateStarkPcs,
};
use p3_dft::Radix2DitParallel;
use p3_field::Field;
use p3_field::extension::BinomialExtensionField;
use p3_fri::{FriParameters, HidingFriPcs};
use p3_matrix::Matrix;
use p3_matrix::dense::RowMajorMatrix;
use p3_merkle_tree::{MerkleTreeHidingMmcs, MerkleTreeMmcs};
use p3_symmetric::{PaddingFreeSponge, TruncatedPermutation};
use rand::SeedableRng;
use rand::rngs::{SmallRng, StdRng};
use rayon::prelude::*;

type Val = BabyBear;
type Challenge = BinomialExtensionField<Val, 4>;
type Perm = Poseidon2BabyBear<16>;
type MyHash = PaddingFreeSponge<Perm, 16, 8, 8>;
type MyCompress = TruncatedPermutation<Perm, 2, 8, 16>;
type Packing = <Val as Field>::Packing;
type ValMmcs = MerkleTreeMmcs<Packing, Packing, MyHash, MyCompress, 2, 8>;
type HidingValMmcs =
    MerkleTreeHidingMmcs<Packing, Packing, MyHash, MyCompress, StdRng, 2, 8, SALT_ELEMS>;
type ChallengeMmcs = ExtensionMmcs<Val, Challenge, ValMmcs>;
type Dft = Radix2DitParallel<Val>;
type Challenger = DuplexChallenger<Val, Perm, 16, 8>;
// The inner MMCS is not hiding on purpose: then bug3 cannot fire inside the bug4/bug5 tests,
// and each test isolates one call site.
type MyPcs = HidingFriPcs<Val, Dft, ValMmcs, ChallengeMmcs, StdRng>;

const SALT_ELEMS: usize = 4;
/// Upstream requires at least `Challenge::DIMENSION` random codewords.
const NUM_RANDOM_CODEWORDS: usize = 4;
/// With 2 queries the hiding budget `N >= 2 * (num_queries + 4 * points)` holds for small traces.
const NUM_QUERIES: usize = 2;
/// Quotient chunks for bug4. Must be greater than 1.
const NUM_CHUNKS: usize = 2;

// ---------------------------------------------------------------------------------------------
// Parameters and watchdog
// ---------------------------------------------------------------------------------------------

struct Params {
    tasks: usize,
    rounds: usize,
    log_height: usize,
    width: usize,
    stall: Duration,
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number, got {v:?}")),
        Err(_) => default,
    }
}

fn params() -> Params {
    Params {
        tasks: env_usize("REPRO_TASKS", 64),
        rounds: env_usize("REPRO_ROUNDS", 20),
        log_height: env_usize("REPRO_LOG_HEIGHT", 14),
        width: env_usize("REPRO_WIDTH", 32),
        stall: Duration::from_secs(env_usize("REPRO_STALL_SECS", 60) as u64),
    }
}

/// bug5's lock window is shorter than bug3/bug4: the lock in `commit` is held only during
/// `with_random_cols`, which is a parallel copy. A larger matrix makes that copy longer.
fn params_bug5() -> Params {
    Params {
        tasks: env_usize("REPRO_TASKS", 128),
        rounds: env_usize("REPRO_ROUNDS", 30),
        log_height: env_usize("REPRO_LOG_HEIGHT", 16),
        width: env_usize("REPRO_WIDTH", 32),
        stall: Duration::from_secs(env_usize("REPRO_STALL_SECS", 120) as u64),
    }
}

/// Runs `task(i)` for `i in 0..p.tasks` on rayon's global pool, `p.rounds` times.
/// Panics (fails the test) if no call completes for `p.stall`.
fn run_concurrently<F>(name: &str, p: &Params, task: F)
where
    F: Fn(usize) + Send + Sync + 'static,
{
    eprintln!(
        "[{name}] workers={} tasks={} rounds={} log_height={} width={} stall={}s",
        rayon::current_num_threads(),
        p.tasks,
        p.rounds,
        p.log_height,
        p.width,
        p.stall.as_secs()
    );

    let done_calls = Arc::new(AtomicUsize::new(0));
    let done_rounds = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<()>();
    let (tasks, rounds) = (p.tasks, p.rounds);
    let (calls, rounds_done) = (done_calls.clone(), done_rounds.clone());

    let start = Instant::now();
    thread::spawn(move || {
        for _ in 0..rounds {
            (0..tasks).into_par_iter().for_each(|i| {
                task(i);
                calls.fetch_add(1, Ordering::Relaxed);
            });
            rounds_done.fetch_add(1, Ordering::Relaxed);
        }
        let _ = tx.send(());
    });

    // A hang is zero progress, not slow progress: the timer restarts at every completed call.
    let mut last_calls = 0;
    let mut last_change = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(()) => {
                eprintln!(
                    "[{name}] OK: {} calls in {:.1?}",
                    done_calls.load(Ordering::Relaxed),
                    start.elapsed()
                );
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("[{name}] a task panicked (see the message above)");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = done_calls.load(Ordering::Relaxed);
                if now != last_calls {
                    last_calls = now;
                    last_change = Instant::now();
                } else if last_change.elapsed() >= p.stall {
                    panic!(
                        "[{name}] DEADLOCK: no call completed in {}s \
                         (round {} of {}, {} of {} calls done, {:.1?} elapsed)",
                        p.stall.as_secs(),
                        done_rounds.load(Ordering::Relaxed) + 1,
                        rounds,
                        now,
                        rounds * tasks,
                        start.elapsed()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

fn perm() -> Perm {
    Perm::new_from_rng_128(&mut SmallRng::seed_from_u64(1))
}

fn val_mmcs() -> ValMmcs {
    let p = perm();
    ValMmcs::new(MyHash::new(p.clone()), MyCompress::new(p), 0)
}

fn hiding_val_mmcs() -> HidingValMmcs {
    let p = perm();
    // Fixed seed because this is a test. Production code must seed from OS entropy.
    HidingValMmcs::new(
        MyHash::new(p.clone()),
        MyCompress::new(p),
        0,
        StdRng::seed_from_u64(7),
    )
}

fn hiding_pcs() -> MyPcs {
    let fri_params = FriParameters {
        log_blowup: 1,
        log_final_poly_len: 0,
        max_log_arity: 1,
        num_queries: NUM_QUERIES,
        batch_proof_of_work_bits: 0,
        commit_proof_of_work_bits: 0,
        query_proof_of_work_bits: 0,
        mmcs: ChallengeMmcs::new(val_mmcs()),
    };
    // Fixed seed because this is a test. Production code must seed from OS entropy.
    MyPcs::new(
        Dft::default(),
        val_mmcs(),
        fri_params,
        NUM_RANDOM_CODEWORDS,
        StdRng::seed_from_u64(7),
    )
}

// ---------------------------------------------------------------------------------------------
// bug3: MerkleTreeHidingMmcs::commit
// ---------------------------------------------------------------------------------------------

/// Upstream holds `self.rng.lock()` until `self.inner.commit(..)` returns.
/// The inner commit builds the Merkle tree in parallel.
#[test]
fn bug3_hiding_mmcs_concurrent_commit() {
    let p = params();
    let mmcs = hiding_val_mmcs();
    let mat =
        RowMajorMatrix::<Val>::rand(&mut SmallRng::seed_from_u64(3), 1 << p.log_height, p.width);
    run_concurrently("bug3 MerkleTreeHidingMmcs::commit", &p, move |_| {
        let _ = mmcs.commit(vec![mat.clone()]);
    });
}

/// Commit, open and verify through the hiding MMCS.
#[test]
fn bug3_hiding_mmcs_roundtrip() {
    let mmcs = hiding_val_mmcs();
    let mat = RowMajorMatrix::<Val>::rand(&mut SmallRng::seed_from_u64(30), 64, 8);
    let dims = vec![mat.dimensions()];
    let (commit, prover_data) = mmcs.commit(vec![mat]);
    for index in [0, 17, 63] {
        let opening = mmcs.open_batch(index, &prover_data);
        mmcs.verify_batch(&commit, &dims, index, (&opening).into())
            .unwrap_or_else(|e| panic!("opening at index {index} must verify: {e:?}"));
    }
}

// ---------------------------------------------------------------------------------------------
// bug4: HidingFriPcs::get_quotient_ldes
// ---------------------------------------------------------------------------------------------

/// Upstream holds `self.rng.lock()` until the function returns: across `with_random_cols`
/// (parallel copy) and across the DFTs. This test uses the same call pattern as
/// `p3-batch-stark`, which calls `get_quotient_ldes` once per AIR instance inside
/// `into_par_iter()`.
#[test]
fn bug4_hiding_pcs_concurrent_get_quotient_ldes() {
    let p = params();
    let pcs = hiding_pcs();
    let trace_domain = <MyPcs as Pcs<Challenge, Challenger>>::natural_domain_for_degree(
        &pcs,
        1 << p.log_height,
    );
    let domains = trace_domain
        .create_disjoint_domain(NUM_CHUNKS << p.log_height)
        .split_domains(NUM_CHUNKS);
    let mut rng = SmallRng::seed_from_u64(4);
    let chunks: Vec<_> = domains
        .iter()
        .map(|d| (*d, RowMajorMatrix::<Val>::rand(&mut rng, d.size(), p.width)))
        .collect();
    run_concurrently("bug4 HidingFriPcs::get_quotient_ldes", &p, move |_| {
        <MyPcs as UnivariateStarkPcs<Challenge, Challenger>>::get_quotient_ldes(
            &pcs,
            chunks.clone(),
            NUM_CHUNKS,
        )
        .expect("get_quotient_ldes must succeed (check the hiding budget)");
    });
}

// ---------------------------------------------------------------------------------------------
// bug5: HidingFriPcs::commit
// ---------------------------------------------------------------------------------------------

/// Upstream passes `&mut *self.rng.lock()` as an argument to `with_random_cols`. The temporary
/// guard lives for the whole call, and `with_random_cols` starts with a parallel copy.
///
/// The lock window is shorter than bug3/bug4 (only the parallel copy), so this test uses
/// a larger matrix. See `params_bug5`.
#[test]
fn bug5_hiding_pcs_concurrent_commit() {
    let p = params_bug5();
    let pcs = hiding_pcs();
    // `commit` interleaves random rows, so the committed domain is twice the trace height.
    let domain = <MyPcs as Pcs<Challenge, Challenger>>::natural_domain_for_degree(
        &pcs,
        2 << p.log_height,
    );
    let trace =
        RowMajorMatrix::<Val>::rand(&mut SmallRng::seed_from_u64(5), 1 << p.log_height, p.width);
    run_concurrently("bug5 HidingFriPcs::commit", &p, move |_| {
        <MyPcs as Pcs<Challenge, Challenger>>::commit(&pcs, [(domain, trace.clone())])
            .expect("commit must succeed (check the hiding budget)");
    });
}

/// Commit, open and verify through the hiding PCS. Same flow as upstream's
/// `make_fixture` in fri/src/hiding_pcs.rs.
#[test]
fn bug5_hiding_pcs_roundtrip() {
    let pcs = hiding_pcs();
    let log_degree = 4;
    let domain =
        <MyPcs as Pcs<Challenge, Challenger>>::natural_domain_for_degree(&pcs, 2 << log_degree);
    let trace = RowMajorMatrix::<Val>::rand(&mut SmallRng::seed_from_u64(50), 1 << log_degree, 4);
    let (commitment, prover_data) =
        <MyPcs as Pcs<Challenge, Challenger>>::commit(&pcs, [(domain, trace)])
            .expect("commit must succeed");

    let mut p_challenger = Challenger::new(perm());
    p_challenger.observe(&commitment);
    let zeta: Challenge = p_challenger.sample_algebra_element();
    let (opened_values, proof) = pcs
        .open(
            vec![OpeningRequest {
                prover_data: &prover_data,
                points: vec![vec![zeta]],
            }],
            &mut p_challenger,
        )
        .expect("open must succeed");

    let mut v_challenger = Challenger::new(perm());
    v_challenger.observe(&commitment);
    let v_zeta: Challenge = v_challenger.sample_algebra_element();
    assert_eq!(v_zeta, zeta, "prover and verifier must sample the same point");

    let claims = vec![(
        commitment,
        vec![(domain, vec![(zeta, opened_values[0][0][0].clone())])],
    )];
    <MyPcs as Pcs<Challenge, Challenger>>::verify(
        &pcs,
        claims.into_iter().map(Into::into).collect(),
        &proof,
        &mut v_challenger,
    )
    .expect("the opening proof must verify");
}