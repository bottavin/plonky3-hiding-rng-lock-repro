# plonky3-hiding-rng-lock-repro

Plonky3 `MerkleTreeHidingMmcs` and `HidingFriPcs` keep their RNG in a `spin::Mutex`.
At three call sites the lock is held while rayon parallel work runs.
When the same instance is used from several rayon tasks, the pool can deadlock.

This project shows the bug on Plonky3 `main` and shows that a small patch fixes it.
The same test file is compiled twice:

| Directory   | Plonky3 source                                                   |
|-------------|------------------------------------------------------------------|
| `upstream/` | git dependency, `main` pinned at `299d81c2db5c2280b57e332e4606376852b23035` |
| `fixed/`    | the same commit with `patches/0001-hiding-rng-lock.patch` applied |

## The three call sites

| Id   | Function                           | File                              | Parallel work under the lock        |
|------|------------------------------------|-----------------------------------|-------------------------------------|
| bug3 | `MerkleTreeHidingMmcs::commit`     | `merkle-tree/src/hiding_mmcs.rs`  | Merkle tree build in `inner.commit` |
| bug4 | `HidingFriPcs::get_quotient_ldes`  | `fri/src/hiding_pcs.rs`           | `with_random_cols`, DFTs            |
| bug5 | `HidingFriPcs::commit`             | `fri/src/hiding_pcs.rs`           | `with_random_cols`                  |

bug4 in upstream code:

```rust
let mut rng = self.rng.lock();   // guard lives until the end of the function
let randomized_evaluations: Vec<RowMajorMatrix<Val>> = evaluations
    .into_iter()
    .map(|mat| mat.with_random_cols(self.num_random_codewords, &mut *rng))
    .collect();
// ... coset_idft_batch / dft_batch run here, still under the lock
```

bug5 in upstream code:

```rust
let mut random_evaluation = mat.with_random_cols(
    mat_width + 2 * self.num_random_codewords,
    &mut *self.rng.lock(),   // temporary guard lives for the whole call
);
```

`with_random_cols` starts with a parallel copy (`par_rows_mut().zip(par_row_slices())`).

## Why it deadlocks

A rayon worker that waits for one of its own sub-tasks does not sleep.
It runs other pending tasks of the pool (work stealing).

1. Worker W takes the spin lock and starts parallel work.
2. Another worker steals one of W's sub-tasks. W must wait for it.
3. While it waits, W picks up another pending task.
   This task calls the same method on the same instance.
4. The task tries to take the lock. W already holds it.
   `spin::Mutex` is not reentrant and never yields, so W spins forever.
5. The other workers also end up spinning on the lock. The pool is dead.

Both types are `Sync`. Upstream even asserts it (`hiding_mmcs_is_sync`).
So calling them from several rayon tasks is valid use.
Upstream `p3-batch-stark` itself calls `pcs.get_quotient_ldes` inside
`(0..n_instances).into_par_iter()` in `batch-stark/src/prover.rs`.

## The fix

Draw the randomness under the lock, then release the lock before any parallel work.

- bug3: scope the lock to the salt generation. `RowMajorMatrix::rand` is sequential.
- bug4, bug5: fork an owned RNG with `StdRng::from_rng(&mut *self.rng.lock())`.
  The temporary guard is dropped at the end of that statement.
  Upstream already uses this pattern in `whir/src/pcs/zk/adapter.rs`.

The fork adds no trait bound, so the public API does not change.
The fork is seeded from the output of the shared `CryptoRng`, as in the existing
`Clone` impls of both types. The mask stream changes, so one upstream unit test that
replays the stream (`expected_quotient_ldes` in `hiding_pcs.rs`) is updated in the patch.

## Tests

`tests/hiding_rng_lock.rs`:

| Test                                             | Checks                                   |
|--------------------------------------------------|------------------------------------------|
| `bug3_hiding_mmcs_concurrent_commit`             | many parallel `commit` calls complete    |
| `bug3_hiding_mmcs_roundtrip`                     | commit, open, verify                     |
| `bug4_hiding_pcs_concurrent_get_quotient_ldes`   | many parallel `get_quotient_ldes` calls complete |
| `bug5_hiding_pcs_concurrent_commit`              | many parallel `commit` calls complete    |
| `bug5_hiding_pcs_roundtrip`                      | commit, open, verify                     |

A watchdog fails a concurrent test when no call completes for `REPRO_STALL_SECS` seconds.
Slow progress is not a failure. Zero progress is.

The bug4 and bug5 tests use a non-hiding inner MMCS, so bug3 cannot fire inside them.
Each test isolates one call site.

Expected result:

| Test group      | `upstream`             | `fixed` |
|-----------------|------------------------|---------|
| `*_concurrent_*`| FAIL (deadlock)        | PASS    |
| `*_roundtrip`   | PASS                   | PASS    |

The deadlock depends on thread scheduling, so a single run on upstream can pass by chance.
The tests use rayon's global pool (one worker per core). They were run on a 32-core machine.

bug3 hangs reliably only with more rayon workers than cores. On a 32-core machine it
passed with 8, 16 and 32 workers and hung with 64 and 128. So `run-tests.sh` runs the
bug3 concurrent test with `RAYON_NUM_THREADS` set to twice the core count, in both modes.
An explicit `RAYON_NUM_THREADS` always wins.

Bug5's lock window is shorter than bug3/bug4: the lock covers only a parallel copy.
That test uses larger defaults.

| Variable           | bug3 / bug4 default | bug5 default |
|--------------------|---------------------|--------------|
| `REPRO_TASKS`      | 64                  | 128          |
| `REPRO_ROUNDS`     | 20                  | 30           |
| `REPRO_LOG_HEIGHT` | 14                  | 16           |
| `REPRO_WIDTH`      | 32                  | 32           |
| `REPRO_STALL_SECS` | 60                  | 120          |

## How to run

**Requirements:** `git`, a recent Rust toolchain, and a machine with many cores.
Plonky3 `main` does not build on Rust 1.91. The tests were run with Rust 1.95.
Check with `rustc --version`.

### Step 1 — clone this repository

```bash
git clone <url>
cd plonky3-hiding-rng-lock-repro
```

Or unzip the archive and enter the directory.

### Step 2 — reproduce the bugs on upstream Plonky3

This downloads Plonky3 `main` from GitHub (pinned at `299d81c2`) and runs the tests.
The three concurrent tests will deadlock and fail. The two roundtrip tests will pass.

```bash
./run-tests.sh upstream
```

Expected summary:

```
PASS  bug3_hiding_mmcs_roundtrip
FAIL  bug3_hiding_mmcs_concurrent_commit        ← deadlock confirmed
FAIL  bug4_hiding_pcs_concurrent_get_quotient_ldes  ← deadlock confirmed
PASS  bug5_hiding_pcs_roundtrip
FAIL  bug5_hiding_pcs_concurrent_commit         ← deadlock confirmed
```

The deadlock tests take several minutes each. The watchdog turns a hang into a test
failure after the stall timeout, then the next test starts.

### Step 3 — create the fixed copy

This clones the same Plonky3 commit into `vendor/plonky3-fixed/` and applies
`patches/0001-hiding-rng-lock.patch`. It does not modify the upstream repo.

```bash
./setup-fixed.sh
```

### Step 4 — verify that the fix works

This runs the same tests against the patched local copy. All five tests should pass.

```bash
./run-tests.sh fixed
```

Expected summary:

```
PASS  bug3_hiding_mmcs_roundtrip
PASS  bug3_hiding_mmcs_concurrent_commit
PASS  bug4_hiding_pcs_concurrent_get_quotient_ldes
PASS  bug5_hiding_pcs_roundtrip
PASS  bug5_hiding_pcs_concurrent_commit
```

Logs are written to `logs/<mode>-<timestamp>.log`.

### Run one test only

```bash
./run-tests.sh upstream bug3_hiding_mmcs_concurrent_commit
```

### Test a different Plonky3 commit

Change `rev` in `upstream/Cargo.toml` and `REV` in `setup-fixed.sh` to the same value.
Delete `vendor/plonky3-fixed` and re-run `./setup-fixed.sh`. The patch may need to be
rebased if the files it touches have changed.

## Background

Found while building qbip32-plonky3, a STARK proof system for BIP-32 key derivation.
There the deadlock appeared as a hang on the second `prove()` call in the same process.