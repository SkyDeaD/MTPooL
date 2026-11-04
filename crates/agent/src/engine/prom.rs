//! The Prometheus text format, as far as telemt's `/metrics` uses it.

/// One line of the exposition: a series and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads the text format: comments and blank lines are skipped, a line that is not a sample is
/// ignored, label values may hold the escapes `\\`, `\"` and `\n`.
pub fn parse_prometheus(text: &str) -> Vec<Sample> {
    text.lines().filter_map(parse_line).collect()
}

/// The sum of every series called `name`; `None` if there is none. A value that is not a finite
/// number is left out of the sum.
pub fn sum(samples: &[Sample], name: &str) -> Option<f64> {
    let mut found = false;
    let mut total = 0.0;
    for sample in samples.iter().filter(|s| s.name == name) {
        found = true;
        if sample.value.is_finite() {
            total += sample.value;
        }
    }
    found.then_some(total)
}

/// The sums of every series called `name` by its `label`: those with one of `values` first, all
/// the others second, a series without the label among them. `None` if there is none. A value
/// that is not a finite number is left out.
pub fn split_sum(
    samples: &[Sample],
    name: &str,
    label: &str,
    values: &[String],
) -> Option<(f64, f64)> {
    let mut found = false;
    let (mut with, mut without) = (0.0, 0.0);
    for sample in samples.iter().filter(|s| s.name == name) {
        found = true;
        if !sample.value.is_finite() {
            continue;
        }
        match sample.label(label) {
            Some(value) if values.iter().any(|v| v == value) => with += sample.value,
            _ => without += sample.value,
        }
    }
    found.then_some((with, without))
}

/// The value of the first series called `name`.
pub fn first(samples: &[Sample], name: &str) -> Option<f64> {
    samples.iter().find(|s| s.name == name).map(|s| s.value)
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let name_end = line
        .find(|c: char| c == '{' || c.is_whitespace())
        .unwrap_or(line.len());
    let name = &line[..name_end];
    if !is_metric_name(name) {
        return None;
    }
    let mut rest = line[name_end..].trim_start();
    let mut labels = Vec::new();
    if let Some(inner) = rest.strip_prefix('{') {
        let (parsed, after) = parse_labels(inner)?;
        labels = parsed;
        rest = after.trim_start();
    }
    // The value, then an optional timestamp.
    let mut parts = rest.split_whitespace();
    let value = parts.next()?.parse::<f64>().ok()?;
    if let Some(timestamp) = parts.next() {
        timestamp.parse::<i64>().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

fn is_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Reads `k="v",k2="v2"}` (the text after the opening brace); returns the labels and what follows
/// the closing brace.
fn parse_labels(mut rest: &str) -> Option<(Vec<(String, String)>, &str)> {
    let mut labels = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if let Some(after) = rest.strip_prefix('}') {
            return Some((labels, after));
        }
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if !is_metric_name(key) {
            return None;
        }
        let quoted = rest[eq + 1..].trim_start().strip_prefix('"')?;
        let mut value = String::new();
        let mut chars = quoted.char_indices();
        let end = loop {
            let (at, c) = chars.next()?;
            match c {
                '"' => break at,
                '\\' => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    other => value.push(other),
                },
                c => value.push(c),
            }
        };
        labels.push((key.to_owned(), value));
        rest = &quoted[end + 1..];
    }
}
