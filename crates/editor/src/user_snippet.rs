use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserSnippet {
    pub name: String,
    pub trigger: String,
    pub language: String,
    pub body: String,
}

#[derive(Clone)]
pub(crate) struct Template {
    pub body: String,
    pub stops: Vec<u16>,
}
enum Part {
    Literal(String),
    Field(u16, Option<String>),
}

impl UserSnippet {
    pub fn label(&self) -> String {
        format!("{} · {}", self.trigger, self.name)
    }
    pub(crate) fn matches(&self, language: &str) -> bool {
        self.language == "*" || self.language.eq_ignore_ascii_case(language)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.trigger.trim().is_empty()
            || self.name.trim().is_empty()
            || self.label().len() > 96
            || self.trigger.chars().any(char::is_whitespace)
            || self.label().chars().any(char::is_control)
        {
            return Err(
                "Enter a name and a single-word trigger. Keep the combined label under 96 bytes."
                    .into(),
            );
        }
        if self.language != "*"
            && !crate::languages()
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&self.language))
        {
            return Err("Choose a supported language or All languages.".into());
        }
        self.template().map(|_| ())
    }
    pub(crate) fn template(&self) -> Result<Template, String> {
        let body = self.body.replace("\r\n", "\n").replace('\r', "\n");
        if body.is_empty() || body.len() > 64 * 1024 || body.contains(['\0', '\x01', '\x02']) {
            return Err("Use a nonempty snippet body of at most 64 KiB.".into());
        }
        let mut chars = body.chars().peekable();
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut defaults = BTreeMap::new();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek().is_some_and(|c| matches!(c, '$' | '\\')) {
                literal.push(chars.next().unwrap());
                continue;
            }
            if c != '$' {
                literal.push(c);
                continue;
            }
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            let mut digits = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                digits.push(chars.next().unwrap());
            }
            if digits.is_empty() {
                literal.push('$');
                if braced {
                    literal.push('{');
                }
                continue;
            }
            let num = digits
                .parse::<u16>()
                .map_err(|_| "Tab stop numbers must fit in 0–65535.")?;
            let mut default = None;
            if braced {
                match chars.next(){
                    Some('}')=>{},
                    Some(':')=>{
                        let mut value=String::new();
                        loop{match chars.next(){
                            Some('}')=>break,
                            Some('\\') if chars.peek().is_some_and(|c|matches!(c,'}'|'\\'))=>value.push(chars.next().unwrap()),
                            Some(c)=>value.push(c),
                            None=>return Err("Close the placeholder with a right brace.".into())
                        }}
                        if value.contains('\n'){return Err("Use one line for a placeholder default.".into());}
                        defaults.entry(num).or_insert_with(||value.clone());default=Some(value);
                    }
                    _=>return Err("A numbered placeholder must end with a right brace or contain a colon and a default.".into()),
                }
            }
            parts.push(Part::Literal(std::mem::take(&mut literal)));
            parts.push(Part::Field(num, default));
            if parts.len() > 1024 {
                return Err("Use at most 512 placeholders per snippet.".into());
            }
        }
        parts.push(Part::Literal(literal));
        let mut body = String::new();
        let mut stops = Vec::new();
        for part in parts {
            match part {
                Part::Literal(s) => body.push_str(&s),
                Part::Field(n, default) => {
                    stops.push(n);
                    body.push('\x01');
                    if let Some(s) = defaults.get(&n).or(default.as_ref()) {
                        body.push_str(s);
                    }
                    body.push('\x02');
                }
            }
        }
        if !stops.contains(&0) {
            stops.push(0);
            body.push_str("\x01\x02");
        }
        Ok(Template { body, stops })
    }
}
