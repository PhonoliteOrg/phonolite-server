#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    Invalid,
    Unsatisfiable,
}

pub fn parse_range_header(value: &str, size: u64) -> Result<ByteRange, RangeError> {
    let value = value.trim();
    if !value.starts_with("bytes=") {
        return Err(RangeError::Invalid);
    }

    if size == 0 {
        return Err(RangeError::Unsatisfiable);
    }

    let range = &value[6..];
    if range.contains(',') {
        return Err(RangeError::Invalid);
    }

    if let Some(suffix) = range.strip_prefix('-') {
        if suffix.is_empty() {
            return Err(RangeError::Invalid);
        }
        let suffix: u64 = suffix.parse().map_err(|_| RangeError::Invalid)?;
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let start = if suffix >= size { 0 } else { size - suffix };
        let end = size - 1;
        return Ok(ByteRange { start, end });
    }

    let mut parts = range.splitn(2, '-');
    let start_str = parts.next().unwrap_or("");
    let end_str = parts.next().unwrap_or("");
    if start_str.is_empty() {
        return Err(RangeError::Invalid);
    }

    let start: u64 = start_str.parse().map_err(|_| RangeError::Invalid)?;
    if start >= size {
        return Err(RangeError::Unsatisfiable);
    }

    let end = if end_str.is_empty() {
        size - 1
    } else {
        let end: u64 = end_str.parse().map_err(|_| RangeError::Invalid)?;
        if end < start {
            return Err(RangeError::Invalid);
        }
        if end >= size {
            size - 1
        } else {
            end
        }
    };

    Ok(ByteRange { start, end })
}

#[cfg(test)]
mod tests {
    use super::{parse_range_header, ByteRange, RangeError};

    #[test]
    fn parses_open_ended_range() {
        let range = parse_range_header("bytes=0-", 100).unwrap();
        assert_eq!(range, ByteRange { start: 0, end: 99 });
    }

    #[test]
    fn parses_closed_range() {
        let range = parse_range_header("bytes=10-19", 100).unwrap();
        assert_eq!(range, ByteRange { start: 10, end: 19 });
    }

    #[test]
    fn clamps_end_overflow() {
        let range = parse_range_header("bytes=90-200", 100).unwrap();
        assert_eq!(range, ByteRange { start: 90, end: 99 });
    }

    #[test]
    fn parses_suffix_range() {
        let range = parse_range_header("bytes=-10", 100).unwrap();
        assert_eq!(range, ByteRange { start: 90, end: 99 });
    }

    #[test]
    fn rejects_multiple_ranges() {
        let err = parse_range_header("bytes=0-1,2-3", 100).unwrap_err();
        assert_eq!(err, RangeError::Invalid);
    }

    #[test]
    fn rejects_invalid_range() {
        let err = parse_range_header("bytes=10-5", 100).unwrap_err();
        assert_eq!(err, RangeError::Invalid);
    }

    #[test]
    fn rejects_unsatisfiable() {
        let err = parse_range_header("bytes=100-", 100).unwrap_err();
        assert_eq!(err, RangeError::Unsatisfiable);
    }
}
