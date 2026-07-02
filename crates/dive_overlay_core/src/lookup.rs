/// Finds the index of the latest sample whose `elapsed_sec` is <= `dive_sec`
/// ("last known value carried forward" semantics), or `None` if `dive_sec`
/// is before the first sample. `times` must be sorted ascending.
///
/// Rust port of the original's `bisect.bisect_right(times, dive_sec) - 1`:
/// `partition_point` with `t <= dive_sec` yields the count of elements
/// `<= dive_sec`, which is exactly `bisect_right`'s result.
pub fn choose_sample_index(times: &[f64], dive_sec: f64) -> Option<usize> {
    let idx = times.partition_point(|&t| t <= dive_sec);
    idx.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_before_first_sample() {
        let times = [10.0, 20.0, 30.0];
        assert_eq!(choose_sample_index(&times, 5.0), None);
    }

    #[test]
    fn returns_exact_match_index() {
        let times = [10.0, 20.0, 30.0];
        assert_eq!(choose_sample_index(&times, 20.0), Some(1));
    }

    #[test]
    fn returns_latest_index_at_or_before() {
        let times = [10.0, 20.0, 30.0];
        assert_eq!(choose_sample_index(&times, 25.0), Some(1));
        assert_eq!(choose_sample_index(&times, 100.0), Some(2));
    }

    #[test]
    fn empty_times_returns_none() {
        let times: [f64; 0] = [];
        assert_eq!(choose_sample_index(&times, 5.0), None);
    }
}
