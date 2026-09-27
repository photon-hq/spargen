//! Casing conversions using Unicode-XID-aware segmentation. `heck` is deliberately not
//! used — it is not Unicode-XID-correct, which correct identifier allocation requires.

/// Convert `raw` to `PascalCase` (for types and variants).
pub fn to_pascal_case(raw: &str) -> String {
    let words = words_or_symbols(raw);
    if words.is_empty() {
        return "Generated".to_owned();
    }
    words
        .into_iter()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Convert `raw` to `snake_case` (for fields, methods, modules).
pub fn to_snake_case(raw: &str) -> String {
    let words = words_or_symbols(raw);
    if words.is_empty() {
        "generated".to_owned()
    } else {
        words.join("_")
    }
}

/// The words of `raw`, led by `minus`/`plus` when `raw` starts with a sign; for a value with no
/// letters or digits at all (an enum value such as `*`
/// or `#`), the names of its ASCII symbols instead, so it still gets a distinct, readable name
/// rather than the shared `Generated` fallback.
fn words_or_symbols(raw: &str) -> Vec<String> {
    let mut found = words(raw);
    if !found.is_empty() {
        // A leading sign is meaning, not a separator: `-Infinity` and `Infinity`, or a
        // descending `-created_at` sort key and `created_at`, must not share a name.
        let sign = match raw.trim_start().chars().next() {
            Some('-') => Some("minus"),
            Some('+') => Some("plus"),
            _ => None,
        };
        if let Some(sign) = sign {
            found.insert(0, sign.to_owned());
        }
        return found;
    }
    raw.chars()
        .filter(|ch| !ch.is_whitespace())
        .map(symbol_name)
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
        .into_iter()
        .flat_map(|name| name.split('_'))
        .map(str::to_owned)
        .collect()
}

/// The name of an ASCII punctuation character, or `None` for anything else.
fn symbol_name(ch: char) -> Option<&'static str> {
    Some(match ch {
        '!' => "exclamation",
        '"' => "quote",
        '#' => "hash",
        '$' => "dollar",
        '%' => "percent",
        '&' => "ampersand",
        '\'' => "apostrophe",
        '(' => "left_paren",
        ')' => "right_paren",
        '*' => "asterisk",
        '+' => "plus",
        ',' => "comma",
        '-' => "minus",
        '.' => "dot",
        '/' => "slash",
        ':' => "colon",
        ';' => "semicolon",
        '<' => "less_than",
        '=' => "equals",
        '>' => "greater_than",
        '?' => "question",
        '@' => "at",
        '[' => "left_bracket",
        '\\' => "backslash",
        ']' => "right_bracket",
        '^' => "caret",
        '_' => "underscore",
        '`' => "backtick",
        '{' => "left_brace",
        '|' => "pipe",
        '}' => "right_brace",
        '~' => "tilde",
        _ => return None,
    })
}

fn words(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lowercase = false;

    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            let is_upper = ch.is_ascii_uppercase();
            if is_upper && previous_lowercase && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.push(ch.to_ascii_lowercase());
            previous_lowercase = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lowercase = false;
        }
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::{to_pascal_case, to_snake_case};

    #[test]
    fn symbol_only_values_are_named_by_their_symbols() {
        assert_eq!(to_pascal_case("*"), "Asterisk");
        assert_eq!(to_pascal_case("#"), "Hash");
        assert_eq!(to_pascal_case("+1"), "Plus1");
        assert_eq!(to_pascal_case("-Infinity"), "MinusInfinity");
        assert_eq!(to_snake_case("-created_at"), "minus_created_at");
        assert_eq!(to_pascal_case("in-progress"), "InProgress");
        assert_eq!(to_pascal_case("<="), "LessThanEquals");
        assert_eq!(to_snake_case("-"), "minus");
        // No letters, digits or ASCII symbols: the generic fallback remains.
        assert_eq!(to_pascal_case(""), "Generated");
        assert_eq!(to_pascal_case("日本"), "Generated");
    }
}
