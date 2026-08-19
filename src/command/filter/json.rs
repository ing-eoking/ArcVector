#[derive(Debug, PartialEq)]
pub(super) enum JsonVal<'a> {
    Str(&'a [u8]),
    Num(f64),
    Bool(bool),
    Null,
}

/// Only top-level fields are addressable.
pub(super) fn lookup<'a>(json: &'a [u8], field: &[u8]) -> Option<JsonVal<'a>> {
    let mut i = 0;
    skip_ws(json, &mut i);
    if i >= json.len() || json[i] != b'{' {
        return None;
    }
    i += 1;

    loop {
        skip_ws(json, &mut i);
        if i >= json.len() {
            return None;
        }
        if json[i] == b'}' {
            return None;
        }
        if json[i] != b'"' {
            return None; // malformed
        }
        let key = scan_string(json, &mut i)?;

        skip_ws(json, &mut i);
        if i >= json.len() || json[i] != b':' {
            return None;
        }
        i += 1;
        skip_ws(json, &mut i);

        if key == field {
            return scan_value(json, &mut i);
        }
        skip_value(json, &mut i)?;

        skip_ws(json, &mut i);
        if i >= json.len() {
            return None;
        }
        // A closing brace means the field is absent; anything else is malformed.
        if json[i] != b',' {
            return None;
        }
        i += 1;
    }
}

fn skip_ws(s: &[u8], i: &mut usize) {
    while *i < s.len() && s[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

/// Consume a quoted string starting at `s[*i] == '"'`, returning its raw body.
fn scan_string<'a>(s: &'a [u8], i: &mut usize) -> Option<&'a [u8]> {
    if *i >= s.len() || s[*i] != b'"' {
        return None;
    }
    *i += 1;
    let start = *i;
    while *i < s.len() {
        match s[*i] {
            b'\\' => *i += 2, // skip the escape pair so \" does not end the string
            b'"' => {
                let body = &s[start..*i];
                *i += 1;
                return Some(body);
            }
            _ => *i += 1,
        }
    }
    None
}

fn scan_value<'a>(s: &'a [u8], i: &mut usize) -> Option<JsonVal<'a>> {
    if *i >= s.len() {
        return None;
    }
    match s[*i] {
        b'"' => scan_string(s, i).map(JsonVal::Str),
        b't' if s[*i..].starts_with(b"true") => {
            *i += 4;
            Some(JsonVal::Bool(true))
        }
        b'f' if s[*i..].starts_with(b"false") => {
            *i += 5;
            Some(JsonVal::Bool(false))
        }
        b'n' if s[*i..].starts_with(b"null") => {
            *i += 4;
            Some(JsonVal::Null)
        }
        b'{' | b'[' => None, // nested values are not addressable in the base grammar
        _ => {
            let start = *i;
            while *i < s.len()
                && !matches!(s[*i], b',' | b'}' | b']')
                && !s[*i].is_ascii_whitespace()
            {
                *i += 1;
            }
            std::str::from_utf8(&s[start..*i])
                .ok()?
                .parse::<f64>()
                .ok()
                .map(JsonVal::Num)
        }
    }
}

fn skip_value(s: &[u8], i: &mut usize) -> Option<()> {
    if *i >= s.len() {
        return None;
    }
    match s[*i] {
        b'"' => {
            scan_string(s, i)?;
            Some(())
        }
        b'{' | b'[' => {
            let mut depth = 0i32;
            while *i < s.len() {
                match s[*i] {
                    b'"' => {
                        scan_string(s, i)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            *i += 1;
                            return Some(());
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
            None
        }
        _ => {
            while *i < s.len() && !matches!(s[*i], b',' | b'}' | b']') {
                *i += 1;
            }
            Some(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &[u8] =
        br#"{"cat":"tech","lang":"ko","ts":1723248000,"score":-1.5,"ok":true,"nil":null}"#;

    #[test]
    fn lookup_does_not_match_a_key_prefix() {
        assert_eq!(lookup(DOC, b"ca"), None);
        assert_eq!(lookup(DOC, b"catx"), None);
    }
}
