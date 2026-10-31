//! Medians and Markdown tables, as the benchmarks' summaries and results
//! pages write them.

/// The median, the mean of the middle two for an even count.
pub fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => Some(f64::midpoint(values[n / 2 - 1], values[n / 2])),
    }
}

/// The median with `None` as larger than every number; `None` if the
/// median falls on one.
pub fn median_failures_last(mut values: Vec<Option<f64>>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        Some(f64::midpoint(values[n / 2 - 1]?, values[n / 2]?))
    }
}

/// A count as a float, for medians.
#[allow(clippy::cast_precision_loss, reason = "counts far below 2^52")]
pub fn lossy(n: u64) -> f64 {
    n as f64
}

/// One Markdown table line.
pub fn table_line(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

/// `value` formatted, or an en dash.
pub fn opt(value: Option<f64>, format: impl Fn(f64) -> String) -> String {
    value.map_or_else(|| "–".to_owned(), format)
}

/// An integer as itself, a half (an even count's median) with one decimal.
pub fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// Rounded, with thousands separators: `12,345`.
pub fn thousands(value: f64) -> String {
    let digits = format!("{:.0}", value.round());
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Text fit for a table cell: one line, no pipes.
pub fn cell(text: &str) -> String {
    text.replace('|', "/").replace('\n', " ")
}

/// Inserts `line` (and a newline) just above `marker` in `doc`; an error
/// naming the marker if `doc` has none. A line already right above the
/// marker's table is not inserted twice, so a report can be rerun.
pub fn insert_above(doc: &mut String, marker: &str, line: &str) -> Result<(), String> {
    let at = doc
        .find(marker)
        .ok_or_else(|| format!("no `{marker}` line in the results page"))?;
    let table_start = doc[..at].rfind("\n\n").map_or(0, |p| p + 2);
    if doc[table_start..at]
        .lines()
        .any(|existing| existing == line)
    {
        return Ok(());
    }
    doc.insert_str(at, &format!("{line}\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_well() {
        assert_eq!(thousands(1_234_567.4), "1,234,567");
        assert_eq!(thousands(999.0), "999");
        assert_eq!(number(3.0), "3");
        assert_eq!(number(3.5), "3.5");
    }

    #[test]
    fn a_line_goes_above_its_marker_once() {
        let mut doc = "## T\n\n| a |\n| --- |\n<!-- m -->\n".to_owned();
        insert_above(&mut doc, "<!-- m -->", "| 1 |").unwrap();
        insert_above(&mut doc, "<!-- m -->", "| 1 |").unwrap();
        assert_eq!(doc, "## T\n\n| a |\n| --- |\n| 1 |\n<!-- m -->\n");
        assert!(insert_above(&mut doc, "<!-- n -->", "| 1 |").is_err());
    }
}
