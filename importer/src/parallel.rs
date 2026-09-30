//! Data parallelism with scoped threads.

use std::thread;

/// `items.iter().map(f).collect()`, on up to `threads` threads.
pub fn map<T: Sync, U: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> U + Sync) -> Vec<U> {
    if threads <= 1 || items.len() <= 1 {
        return items.iter().map(f).collect();
    }
    let per_thread = items.len().div_ceil(threads);
    let f = &f;
    thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(per_thread)
            .map(|chunk| {
                thread::Builder::new()
                    .name("worker".into())
                    .spawn_scoped(s, move || chunk.iter().map(f).collect::<Vec<U>>())
                    .expect("cannot start a thread")
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("a worker thread panicked"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_order() {
        let items: Vec<u32> = (0..1000).collect();
        assert_eq!(
            super::map(&items, 7, |x| x * 2),
            items.iter().map(|x| x * 2).collect::<Vec<_>>()
        );
        assert!(super::map(&[] as &[u32], 4, |x| *x).is_empty());
    }
}
