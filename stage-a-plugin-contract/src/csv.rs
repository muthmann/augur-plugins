//! Minimal CSV record splitting, shared by the protocol reader and the sensor
//! readout compactor.
//!
//! Deliberately not a CSV *library*: both callers read small files this plugin
//! or the host wrote, and both locate their columns by header name rather than
//! by position. What is actually needed is one correct field splitter — the
//! doubled-quote escaping the host emits, and a quoted `label` in a
//! hand-written protocol, are the only cases that are not `split(',')`.

/// Splits one CSV record, honouring `"…"` quoting and `""` as an escaped quote.
pub fn split_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            }
            '"' => quoted = true,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_fields_split_on_commas() {
        assert_eq!(split_line("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(split_line("a,,c"), vec!["a", "", "c"]);
        assert_eq!(split_line(""), vec![""]);
    }

    #[test]
    fn quoted_fields_keep_their_commas() {
        assert_eq!(split_line("a,\"b,c\",d"), vec!["a", "b,c", "d"]);
    }

    #[test]
    fn doubled_quotes_are_one_literal_quote() {
        assert_eq!(split_line("\"a\"\"b\",c"), vec!["a\"b", "c"]);
    }

    #[test]
    fn an_unterminated_quote_takes_the_rest_of_the_line() {
        // Better than dropping the row: the caller validates the fields it
        // needs, and a truncated line is reported there with its line number.
        assert_eq!(split_line("a,\"b,c"), vec!["a", "b,c"]);
    }
}
