//! A subset of foobar2000's title formatting, for the Misskey note template:
//! `%field%`, `$function(arg,...)`, `[optional part]` and `'literal text'`.
//!
//! As in foobar2000, every piece has a text and a truth value: a field is true when the track
//! has it, `[...]` shows only when a field inside it is true, and `$if` tests the truth value.
//! Missing fields expand to nothing (foobar2000 shows `?`).

/// Looks up a field by its lowercased name; `$info(x)` asks for `__x`.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug)]
enum Node {
    Text(String),
    Field(String),
    Call(String, Vec<Vec<Node>>),
    Optional(Vec<Node>),
}

pub fn format(template: &str, lookup: Lookup) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut pos = 0;
    let nodes = parse(&chars, &mut pos, Stop::End);
    eval(&nodes, lookup).0
}

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    End,
    /// Inside `$f(...)`: `,` and `)` end an argument.
    Arg,
    /// Inside `[...]`.
    Bracket,
}

fn parse(c: &[char], pos: &mut usize, stop: Stop) -> Vec<Node> {
    let mut out = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Node>| {
        if !text.is_empty() {
            out.push(Node::Text(std::mem::take(text)));
        }
    };
    while *pos < c.len() {
        let ch = c[*pos];
        match ch {
            ',' | ')' if stop == Stop::Arg => break,
            ']' if stop == Stop::Bracket => break,
            '\'' => {
                // '' is a quote; otherwise everything up to the next quote is literal.
                if c.get(*pos + 1) == Some(&'\'') {
                    text.push('\'');
                    *pos += 2;
                    continue;
                }
                match c[*pos + 1..].iter().position(|&x| x == '\'') {
                    Some(n) => {
                        text.extend(&c[*pos + 1..*pos + 1 + n]);
                        *pos += n + 2;
                    }
                    None => {
                        text.extend(&c[*pos + 1..]);
                        *pos = c.len();
                    }
                }
            }
            '%' => match c[*pos + 1..].iter().position(|&x| x == '%') {
                Some(n) if n > 0 => {
                    flush(&mut text, &mut out);
                    out.push(Node::Field(c[*pos + 1..*pos + 1 + n].iter().collect::<String>().to_lowercase()));
                    *pos += n + 2;
                }
                // %% or a lone % is a percent sign.
                Some(_) => {
                    text.push('%');
                    *pos += 2;
                }
                None => {
                    text.push('%');
                    *pos += 1;
                }
            },
            '$' => {
                let start = *pos + 1;
                let mut end = start;
                while end < c.len() && (c[end].is_ascii_alphanumeric() || c[end] == '_') {
                    end += 1;
                }
                if end > start && c.get(end) == Some(&'(') {
                    flush(&mut text, &mut out);
                    let name: String = c[start..end].iter().collect::<String>().to_lowercase();
                    *pos = end + 1;
                    let mut args = Vec::new();
                    loop {
                        args.push(parse(c, pos, Stop::Arg));
                        match c.get(*pos) {
                            Some(',') => *pos += 1,
                            Some(')') => {
                                *pos += 1;
                                break;
                            }
                            _ => break,
                        }
                    }
                    // $f() has no arguments rather than one empty one.
                    if args.len() == 1 && args[0].is_empty() {
                        args.clear();
                    }
                    out.push(Node::Call(name, args));
                } else if c.get(start) == Some(&'$') {
                    text.push('$');
                    *pos += 2;
                } else {
                    text.push('$');
                    *pos += 1;
                }
            }
            '[' => {
                flush(&mut text, &mut out);
                *pos += 1;
                let inner = parse(c, pos, Stop::Bracket);
                if c.get(*pos) == Some(&']') {
                    *pos += 1;
                }
                out.push(Node::Optional(inner));
            }
            _ => {
                text.push(ch);
                *pos += 1;
            }
        }
    }
    flush(&mut text, &mut out);
    out
}

/// Text and truth value.
type Value = (String, bool);

fn eval(nodes: &[Node], lookup: Lookup) -> Value {
    let mut text = String::new();
    let mut truth = false;
    for n in nodes {
        let (t, b) = match n {
            Node::Text(s) => (s.clone(), false),
            Node::Field(name) => match lookup(name).filter(|v| !v.is_empty()) {
                Some(v) => (v, true),
                None => (String::new(), false),
            },
            Node::Optional(inner) => {
                let v = eval(inner, lookup);
                if v.1 { v } else { (String::new(), false) }
            }
            Node::Call(name, args) => call(name, args, lookup),
        };
        text.push_str(&t);
        truth |= b;
    }
    (text, truth)
}

/// The leading integer of `s`, as foobar2000 reads numbers ("3/12" is 3).
fn number(s: &str) -> i64 {
    let s = s.trim();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let n = digits.chars().take_while(char::is_ascii_digit).fold(0i64, |n, d| n.saturating_mul(10).saturating_add(d as i64 - '0' as i64));
    if neg { -n } else { n }
}

fn call(name: &str, args: &[Vec<Node>], lookup: Lookup) -> Value {
    let arg = |i: usize| args.get(i).map(|a| eval(a, lookup)).unwrap_or_default();
    let num = |i: usize| number(&arg(i).0);
    let truth = |b: bool| (String::new(), b);
    let numeric = |n: i64| (n.to_string(), false);
    // String functions keep the truth of what they change.
    let map = |f: &dyn Fn(&str) -> String| {
        let (s, b) = arg(0);
        (f(&s), b)
    };
    match (name, args.len()) {
        ("if", 2 | 3) => if arg(0).1 { arg(1) } else { arg(2) },
        ("if2", 2) => {
            let a = arg(0);
            if a.1 { a } else { arg(1) }
        }
        ("if3", n) if n >= 2 => (0..n - 1).map(arg).find(|v| v.1).unwrap_or_else(|| arg(n - 1)),
        ("ifequal", 4) => if num(0) == num(1) { arg(2) } else { arg(3) },
        ("ifgreater", 4) => if num(0) > num(1) { arg(2) } else { arg(3) },
        ("iflonger", 4) => if arg(0).0.chars().count() as i64 > num(1) { arg(2) } else { arg(3) },
        ("not", 1) => truth(!arg(0).1),
        ("and", _) => truth((0..args.len()).all(|i| arg(i).1)),
        ("or", _) => truth((0..args.len()).any(|i| arg(i).1)),
        ("strcmp", 2) => truth(arg(0).0 == arg(1).0),
        ("stricmp", 2) => truth(arg(0).0.to_lowercase() == arg(1).0.to_lowercase()),
        ("greater", 2) => truth(num(0) > num(1)),
        ("add", _) => numeric((0..args.len()).map(num).fold(0, i64::saturating_add)),
        ("mul", _) => numeric((0..args.len()).map(num).fold(1, i64::saturating_mul)),
        ("sub", n) if n >= 1 => numeric((1..n).map(num).fold(num(0), i64::saturating_sub)),
        // As in foobar2000, dividing by zero leaves the number alone.
        ("div", 2) => numeric(if num(1) == 0 { num(0) } else { num(0) / num(1) }),
        ("mod", 2) => numeric(if num(1) == 0 { num(0) } else { num(0) % num(1) }),
        ("min", n) if n >= 1 => numeric((0..n).map(num).min().unwrap_or(0)),
        ("max", n) if n >= 1 => numeric((0..n).map(num).max().unwrap_or(0)),
        ("left" | "cut", 2) => {
            let n = num(1).max(0) as usize;
            map(&|s| s.chars().take(n).collect())
        }
        ("right", 2) => {
            let n = num(1).max(0) as usize;
            map(&|s| {
                let len = s.chars().count();
                s.chars().skip(len.saturating_sub(n)).collect()
            })
        }
        ("len", 1) => numeric(arg(0).0.chars().count() as i64),
        ("upper", 1) => map(&|s| s.to_uppercase()),
        ("lower", 1) => map(&|s| s.to_lowercase()),
        ("caps", 1) => map(&|s| {
            let mut up = true;
            s.chars()
                .flat_map(|ch| {
                    let out: Vec<char> = if up { ch.to_uppercase().collect() } else { ch.to_lowercase().collect() };
                    up = ch.is_whitespace();
                    out
                })
                .collect()
        }),
        ("trim", 1) => map(&|s| s.trim().to_string()),
        ("num", 2) => {
            let width = num(1).max(0) as usize;
            let (s, b) = arg(0);
            (format!("{:0width$}", number(&s)), b)
        }
        ("pad", 2 | 3) => {
            let width = num(1).max(0) as usize;
            let fill = arg(2).0.chars().next().unwrap_or(' ');
            map(&|s| {
                let len = s.chars().count();
                let mut out = s.to_string();
                out.extend(std::iter::repeat_n(fill, width.saturating_sub(len)));
                out
            })
        }
        ("replace", n) if n >= 3 && n % 2 == 1 => {
            let pairs: Vec<(String, String)> = (1..n).step_by(2).map(|i| (arg(i).0, arg(i + 1).0)).collect();
            map(&|s| pairs.iter().filter(|(from, _)| !from.is_empty()).fold(s.to_string(), |s, (from, to)| s.replace(from, to)))
        }
        ("crlf", 0) => ("\n".into(), false),
        ("tab", 0) => ("\t".into(), false),
        ("info", 1) => field(&format!("__{}", arg(0).0.trim().to_lowercase()), lookup),
        ("meta", 1) => field(&arg(0).0.trim().to_lowercase(), lookup),
        _ => (format!("[unknown function ${name} with {} arguments]", args.len()), false),
    }
}

fn field(name: &str, lookup: Lookup) -> Value {
    match lookup(name).filter(|v| !v.is_empty()) {
        Some(v) => (v, true),
        None => (String::new(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(t: &str) -> String {
        let fields = [
            ("title", "Song"),
            ("artist", "Band"),
            ("samplerate", "44100"),
            ("__samplerate", "44100"),
            ("codec", "AAC"),
            ("__bitspersample", ""),
            ("tracknumber", "03"),
        ];
        format(t, &|name| fields.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string()))
    }

    #[test]
    fn fields_and_literals() {
        assert_eq!(fmt("%artist% - %title%"), "Band - Song");
        assert_eq!(fmt("%album%"), "");
        assert_eq!(fmt("'%title%' is 100%% it''s"), "%title% is 100% it's");
        // An unpaired quote makes the rest literal.
        assert_eq!(fmt("it's %title%"), "its %title%");
        assert_eq!(fmt("''"), "'");
        assert_eq!(fmt("a, b) c"), "a, b) c");
    }

    #[test]
    fn optional_parts_need_a_field() {
        assert_eq!(fmt("[%album% - ]%title%"), "Song");
        assert_eq!(fmt("[%artist% - ]%title%"), "Band - Song");
        assert_eq!(fmt("[no fields]"), "");
    }

    #[test]
    fn functions() {
        assert_eq!(fmt("$if(%album%,yes,no)"), "no");
        assert_eq!(fmt("$if(%title%,yes,no)"), "yes");
        assert_eq!(fmt("$if2(%album%,%title%)"), "Song");
        assert_eq!(fmt("$if3(%album%,%nope%,%artist%,x)"), "Band");
        assert_eq!(fmt("$if($stricmp(%codec%,aac),16 bit,N/A)"), "16 bit");
        assert_eq!(fmt("$div(%samplerate%,1000).$left($right(0$mod(%samplerate%,1000),3),1) kHz"), "44.1 kHz");
        assert_eq!(fmt("$num(%tracknumber%,3) $upper(%title%) $len(%artist%)"), "003 SONG 4");
        assert_eq!(fmt("$if(%codec%, '('%codec%')',)"), " (AAC)");
        assert_eq!(fmt("$replace(%title%,o,0,S,5)"), "50ng");
        assert_eq!(fmt("$if($info(bitspersample),$info(bitspersample) bit,N/A)"), "N/A");
        assert_eq!(fmt("$caps(hello wORLD)"), "Hello World");
        assert_eq!(fmt("a$crlf()b"), "a\nb");
        assert_eq!(fmt("$div(5,0)"), "5");
        assert!(fmt("$nope(1)").contains("unknown function $nope"));
    }

    #[test]
    fn broken_templates_dont_panic() {
        for t in ["$if(", "[%title%", "'open", "%open", "$", "$if(a,b", "$$x", "]", ")"] {
            let _ = fmt(t);
        }
        assert_eq!(fmt("$$x"), "$x");
    }
}
