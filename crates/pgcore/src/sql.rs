//! SQL text helpers. Every identifier that reaches a query goes through [`quote_ident`]; values are
//! never interpolated (they travel as bind parameters).

/// Quotes an identifier the way `quote_ident()` does when quoting is required, but always quotes:
/// wraps in double quotes and doubles embedded double quotes.
pub fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_plain_and_awkward_names() {
        assert_eq!(quote_ident("customers"), "\"customers\"");
        assert_eq!(quote_ident("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident(""), "\"\"");
        assert_eq!(quote_ident("ünï-cødé"), "\"ünï-cødé\"");
    }

    #[test]
    fn injection_attempts_stay_inside_the_quotes() {
        let evil = "x\"; DROP TABLE users; --";
        let q = quote_ident(evil);
        assert_eq!(q, "\"x\"\"; DROP TABLE users; --\"");
        // Every quote inside the identifier is doubled, so the quoted form is one token.
        let inner = &q[1..q.len() - 1];
        assert!(!inner.replace("\"\"", "").contains('"'));
    }

    #[test]
    fn qualifies() {
        assert_eq!(quote_qualified("shop", "orders"), "\"shop\".\"orders\"");
    }
}
