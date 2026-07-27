pub fn percent(used: u64, capacity: u64) -> Option<u8> {
    if capacity == 0 {
        return None;
    }

    let value = (u128::from(used) * 100 / u128::from(capacity)).min(100);
    Some(value as u8)
}

#[cfg(test)]
mod tests {
    use super::percent;

    #[test]
    fn context_pressure_is_bounded_and_optional() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(0, 100), Some(0));
        assert_eq!(percent(50, 100), Some(50));
        assert_eq!(percent(u64::MAX, u64::MAX), Some(100));
        assert_eq!(percent(u64::MAX, 1), Some(100));
    }
}
