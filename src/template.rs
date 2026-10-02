//! Minimal `{var}` substitution for the LCD screen text in
//! `[templates.*]`. Deliberately not a real template engine: no
//! conditionals, loops, or format specifiers -- every value arrives here
//! already formatted (`hal.rs` does the rounding/unit conversion), and the
//! few places that used to branch (HDD with/without a temperature, IP vs.
//! link text) expose a pre-resolved variable instead. `{{` and `}}` are
//! literal braces.

/// Placeholder names a template uses, or an error for an unclosed `{` /
/// stray `}`. Used at config load to reject typos up front rather than
/// showing `{tmep}` on the panel.
pub fn placeholders(tpl: &str) -> Result<Vec<&str>, String> {
    let mut names = Vec::new();
    let mut rest = tpl;
    while let Some(i) = rest.find(['{', '}']) {
        let (c, after) = (rest.as_bytes()[i], &rest[i + 1..]);
        if after.as_bytes().first() == Some(&c) {
            rest = &after[1..]; // `{{` or `}}`
            continue;
        }
        if c == b'}' {
            return Err(format!("stray '}}' in \"{tpl}\" (use '}}}}' for a literal brace)"));
        }
        let end = after.find('}').ok_or_else(|| format!("unclosed '{{' in \"{tpl}\""))?;
        names.push(&after[..end]);
        rest = &after[end + 1..];
    }
    Ok(names)
}

/// Fills in `{name}` from `vars`, then trims trailing whitespace so an
/// empty trailing variable (e.g. `{temp}` with no reading) doesn't leave a
/// dangling space that would lengthen the line for scrolling. Unknown
/// names are left as-is -- `TemplatesConfig::validate` has already
/// rejected those, so this only happens for a template that was never
/// validated.
pub fn render(tpl: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let (c, after) = (rest.as_bytes()[i], &rest[i + 1..]);
        if after.as_bytes().first() == Some(&c) {
            out.push(c as char);
            rest = &after[1..];
            continue;
        }
        match (c, after.find('}')) {
            (b'{', Some(end)) => {
                let name = &after[..end];
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[i..i + end + 2]),
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push(c as char);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_and_trims() {
        let vars = [("status", "PASSED"), ("temp", "")];
        assert_eq!(render("{status} {temp}", &vars), "PASSED");
        assert_eq!(render("{status} {temp}", &[("status", "OK"), ("temp", "34C")]), "OK 34C");
    }

    #[test]
    fn literal_braces() {
        assert_eq!(render("{{{a}}}", &[("a", "x")]), "{x}");
        assert_eq!(placeholders("{{{a}}}").unwrap(), vec!["a"]);
    }

    #[test]
    fn unknown_left_verbatim() {
        assert_eq!(render("a {nope} b", &[]), "a {nope} b");
    }

    #[test]
    fn placeholder_errors() {
        assert_eq!(placeholders("BAY{bay} {dev}").unwrap(), vec!["bay", "dev"]);
        assert!(placeholders("oops {name").is_err());
        assert!(placeholders("oops } x").is_err());
    }
}
