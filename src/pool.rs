//! A tiny scoped thread pool for short blocking jobs (HTTP requests, image
//! decoding). Threads live only as long as one call, so nothing idles.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Runs `f` over `items` on up to `workers` scoped threads and returns the
/// results in the order of `items`.
pub fn par_map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let n = workers.max(1).min(items.len());
    let mut out: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(i) else {
                            return done;
                        };
                        done.push((i, f(item)));
                    }
                })
            })
            .collect();
        for h in handles {
            for (i, r) in h.join().expect("pool worker panicked") {
                out[i] = Some(r);
            }
        }
    });
    out.into_iter()
        .map(|r| r.expect("every item was processed"))
        .collect()
}

/// Runs `f` over `items` on up to `workers` scoped threads. `f` usually
/// reports each result itself (to the UI thread), as soon as it has it.
pub fn par_for_each<T: Sync>(items: &[T], workers: usize, f: impl Fn(&T) + Sync) {
    par_map(items, workers, f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn maps_every_item_once_in_order() {
        let calls = AtomicUsize::new(0);
        let items: Vec<u32> = (0..100).collect();
        let out = par_map(&items, 8, |x| {
            calls.fetch_add(1, Ordering::Relaxed);
            x * 2
        });
        assert_eq!(out, items.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert_eq!(calls.into_inner(), 100);
    }

    #[test]
    fn runs_items_in_parallel_up_to_the_worker_limit() {
        // The first three items wait (up to a deadline) until all three are
        // running, so the peak is exactly 3 only if the pool runs them at once
        // and never starts a fourth alongside.
        let arrived = AtomicUsize::new(0);
        let (running, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let items: Vec<usize> = (0..12).collect();
        par_for_each(&items, 3, |&i| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            if i < 3 {
                arrived.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + Duration::from_secs(5);
                while arrived.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
            }
            std::thread::sleep(Duration::from_millis(2));
            running.fetch_sub(1, Ordering::SeqCst);
        });
        assert_eq!(peak.into_inner(), 3);
    }

    #[test]
    fn handles_empty_input_and_odd_worker_counts() {
        assert!(par_map(&[] as &[u8], 8, |x| *x).is_empty());
        // More workers than items, or none asked for: still does the work.
        assert_eq!(par_map(&[1, 2], 16, |x| x + 1), vec![2, 3]);
        assert_eq!(par_map(&[1, 2], 0, |x| x + 1), vec![2, 3]);
    }
}
