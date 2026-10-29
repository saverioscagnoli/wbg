const GAP: usize = 8;

pub fn encode(prev: &[u8], curr: &[u8]) -> (Vec<(u32, u32)>, Vec<u8>) {
    let (a, _) = prev.as_chunks::<4>();
    let (b, _) = curr.as_chunks::<4>();

    let mut spans = Vec::new();
    let mut data = Vec::new();
    let mut i = 0;

    while i < b.len() {
        if a[i] == b[i] {
            i += 1;
            continue;
        }

        let start = i;
        let mut end = i + 1;

        while end < b.len() {
            let next = b[end..]
                .iter()
                .zip(&a[end..])
                .take(GAP)
                .position(|(x, y)| x != y);

            match next {
                Some(k) => end += k + 1,
                None => break,
            }
        }

        spans.push(((start * 4) as u32, ((end - start) * 4) as u32));
        data.extend_from_slice(&curr[start * 4..end * 4]);
        i = end;
    }

    (spans, lz4_flex::compress(&data))
}

pub fn trim() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::malloc_trim(0);
    }
}
