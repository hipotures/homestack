//! Surgery on the two HomeStack-owned description blocks. User whitespace
//! is preserved; only one adjacent LF separator belongs to a removed block.

use std::ops::Range;

use crate::CoreError;

pub(crate) fn find<'a>(
    description: &'a str,
    start_marker: &str,
    end_marker: &str,
    label: &str,
) -> Result<Option<(Range<usize>, &'a str)>, CoreError> {
    let Some(start) = description.find(start_marker) else {
        return Ok(None);
    };
    let payload_start = start + start_marker.len();
    let end = description[payload_start..]
        .find(end_marker)
        .map(|offset| payload_start + offset)
        .ok_or_else(|| CoreError::Unusable(format!("The {label} in the VM description is cut off")))?;
    if description[payload_start..end].contains("<!-- homestack-") {
        return Err(CoreError::Unusable(format!(
            "The {label} in the VM description is cut off"
        )));
    }
    Ok(Some((start..end + end_marker.len(), &description[payload_start..end])))
}

pub(crate) fn with_block(
    current: &str,
    start: &str,
    end: &str,
    label: &str,
    payload: Option<&str>,
) -> Result<String, CoreError> {
    let found = find(current, start, end, label)?;
    if let Some(payload) = payload {
        let block = format!("{start}{payload}{end}");
        if let Some((range, _)) = found {
            let mut result = current.to_owned();
            result.replace_range(range, &block);
            return Ok(result);
        }
        let separator = if current.is_empty() { "" } else { "\n" };
        return Ok(format!("{current}{separator}{block}"));
    }
    let Some((range, _)) = found else {
        return Ok(current.to_owned());
    };
    let mut before = &current[..range.start];
    let mut after = &current[range.end..];
    if before.is_empty() {
        after = after.strip_prefix('\n').unwrap_or(after);
    } else if after.is_empty() || after.starts_with('\n') {
        before = before.strip_suffix('\n').unwrap_or(before);
    }
    // In the middle, retain a line boundary rather than joining user lines.
    Ok(format!("{before}{after}"))
}
