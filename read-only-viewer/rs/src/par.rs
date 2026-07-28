//! Parallel map over files, on the standard library only.
//!
//! This is the part Python cannot follow. Scanning transcripts is pure CPU
//! (JSON parsing) over independent files, which is exactly what the GIL
//! serialises — `multiprocessing` would work but pays a pickle round-trip of
//! every result.
//!
//! Results are returned in input order, so output stays deterministic and the
//! equivalence gate still means something.

pub fn pmap<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    // Sync, not Send: the workers share `&[T]` rather than owning the items.
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let n = items.len();
    if n < 2 {
        return items.iter().map(|i| f(i)).collect();
    }
    let workers = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1).min(n);
    if workers < 2 {
        return items.iter().map(|i| f(i)).collect();
    }

    // Chunk rather than work-steal: transcripts vary in size, but a chunked
    // split is within noise here and needs no shared queue or dependency.
    let chunk = n.div_ceil(workers);
    let slices: Vec<&[T]> = items.chunks(chunk).collect();
    let f = &f;

    let mut out: Vec<R> = Vec::with_capacity(n);
    std::thread::scope(|scope| {
        let handles: Vec<_> = slices
            .into_iter()
            .map(|slice| scope.spawn(move || slice.iter().map(|i| f(i)).collect::<Vec<R>>()))
            .collect();
        for handle in handles {
            // A panicking worker would otherwise be swallowed and silently
            // shorten the result, which reads as "those sessions do not exist".
            match handle.join() {
                Ok(part) => out.extend(part),
                Err(_) => std::process::abort(),
            }
        }
    });
    out
}
