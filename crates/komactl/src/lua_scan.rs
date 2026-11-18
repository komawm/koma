//! Dependency-free lexical scan of Koma's `init.lua` tree.
//!
//! `komactl doctor` needs two facts that the sandbox evaluation cannot report because the
//! schema silently drops them: which `require("...")` targets the configuration names, and
//! which table keys appear in configuration tables that the evaluated registry never exposes.
//! Both are answered here by a tokenizer that understands Lua comments, quoted strings, and
//! long brackets (`[[...]]`, `[==[...]==]`), so a key that appears inside a comment or a
//! string is never mistaken for configuration.

/// One lexical token, tagged with the 1-based source line it started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Ident(String),
    Str(String),
    Punct(char),
    Number,
}

impl Token {
    fn ident(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(name) => Some(name),
            _ => None,
        }
    }

    fn punct(&self) -> Option<char> {
        match self.kind {
            TokenKind::Punct(ch) => Some(ch),
            _ => None,
        }
    }
}

/// A `require(...)` call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequireTarget {
    pub module: String,
    pub line: usize,
}

/// A table key found in one argument of a `koma.<call>(...)` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableKey {
    pub call: String,
    /// Zero-based argument index inside the call parentheses.
    pub arg_index: usize,
    pub key: String,
    pub line: usize,
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

/// Length in bytes of a long-bracket opener (`[`, `[=`, `[==`, ... `[`) at `bytes[at]`.
fn long_bracket_level(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    if bytes.get(at) != Some(&b'[') {
        return None;
    }
    let mut cursor = at + 1;
    let mut level = 0;
    while bytes.get(cursor) == Some(&b'=') {
        level += 1;
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'[') {
        Some((level, cursor + 1))
    } else {
        None
    }
}

/// Skips a long comment/string body; `from` is the first byte after the opener.
fn skip_long_body(source: &str, from: usize, level: usize, line: &mut usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = from;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\n' {
            *line += 1;
        }
        if bytes[cursor] == b']' {
            let mut probe = cursor + 1;
            let mut seen = 0;
            while bytes.get(probe) == Some(&b'=') {
                seen += 1;
                probe += 1;
            }
            if seen == level && bytes.get(probe) == Some(&b']') {
                return probe + 1;
            }
        }
        cursor += 1;
    }
    bytes.len()
}

/// Tokenizes Lua source, discarding comments and whitespace.
pub(crate) fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    let mut line = 1;

    while cursor < bytes.len() {
        let byte = bytes[cursor];

        if byte == b'\n' {
            line += 1;
            cursor += 1;
            continue;
        }
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }

        // `--` line comment or `--[[ ... ]]` block comment.
        if byte == b'-' && bytes.get(cursor + 1) == Some(&b'-') {
            if let Some((level, body)) = long_bracket_level(bytes, cursor + 2) {
                cursor = skip_long_body(source, body, level, &mut line);
            } else {
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            continue;
        }

        // Long string literal `[[ ... ]]`.
        if byte == b'[' {
            if let Some((level, body)) = long_bracket_level(bytes, cursor) {
                let start_line = line;
                let end = skip_long_body(source, body, level, &mut line);
                let content = source.get(body..end.saturating_sub(level + 2));
                tokens.push(Token {
                    kind: TokenKind::Str(content.unwrap_or_default().to_owned()),
                    line: start_line,
                });
                cursor = end;
                continue;
            }
            tokens.push(Token {
                kind: TokenKind::Punct('['),
                line,
            });
            cursor += 1;
            continue;
        }

        // Quoted string literal with Lua escape sequences.
        if byte == b'"' || byte == b'\'' {
            let quote = byte;
            let start_line = line;
            cursor += 1;
            let mut content = String::new();
            while cursor < bytes.len() {
                let ch = bytes[cursor];
                if ch == b'\\' {
                    // Copy the escaped character verbatim; the sandbox performs real
                    // evaluation, this scan only needs string identity.
                    if let Some(next) = bytes.get(cursor + 1) {
                        if *next == b'\n' {
                            line += 1;
                        } else {
                            content.push(*next as char);
                        }
                        cursor += 2;
                        continue;
                    }
                    break;
                }
                if ch == quote {
                    cursor += 1;
                    break;
                }
                if ch == b'\n' {
                    line += 1;
                }
                content.push(ch as char);
                cursor += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Str(content),
                line: start_line,
            });
            continue;
        }

        if is_ident_start(byte as char) {
            let start = cursor;
            while cursor < bytes.len() && is_ident_continue(bytes[cursor] as char) {
                cursor += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Ident(source[start..cursor].to_owned()),
                line,
            });
            continue;
        }

        if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(cursor + 1).is_some_and(u8::is_ascii_digit))
        {
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric()
                    || bytes[cursor] == b'.'
                    || bytes[cursor] == b'_'
                    || ((bytes[cursor] == b'+' || bytes[cursor] == b'-')
                        && cursor > 0
                        && matches!(bytes[cursor - 1], b'e' | b'E')))
            {
                cursor += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Number,
                line,
            });
            continue;
        }

        let ch = source[cursor..].chars().next().unwrap_or('?');
        tokens.push(Token {
            kind: TokenKind::Punct(ch),
            line,
        });
        cursor += ch.len_utf8();
    }

    tokens
}

/// Every literal module name passed to `require("...")` or `require "..."`.
pub(crate) fn require_targets(tokens: &[Token]) -> Vec<RequireTarget> {
    let mut targets = Vec::new();
    for window in tokens.windows(3) {
        if window[0].ident() != Some("require") {
            continue;
        }
        if let TokenKind::Str(module) = &window[2].kind
            && matches!(window[1].punct(), Some('('))
            && !module.trim().is_empty()
        {
            targets.push(RequireTarget {
                module: module.trim().to_owned(),
                line: window[0].line,
            });
        }
    }
    for window in tokens.windows(2) {
        if window[0].ident() == Some("require")
            && let TokenKind::Str(module) = &window[1].kind
            && !module.trim().is_empty()
        {
            let duplicate = targets
                .iter()
                .any(|t| t.module == module.trim() && t.line == window[0].line);
            if !duplicate {
                targets.push(RequireTarget {
                    module: module.trim().to_owned(),
                    line: window[0].line,
                });
            }
        }
    }
    targets.sort_by(|a, b| (a.line, &a.module).cmp(&(b.line, &b.module)));
    targets
}

/// Table keys of the `arg_index`-th argument of every `koma.<call>(...)` listed in `calls`.
///
/// Only literal table constructors in that exact argument are inspected, so a table built by
/// a helper function or passed through a variable never produces a false positive.
pub(crate) fn table_keys_for_calls(tokens: &[Token], calls: &[&str]) -> Vec<TableKey> {
    let mut keys = Vec::new();
    for start in 0..tokens.len() {
        if tokens[start].ident() != Some("koma")
            || tokens.get(start + 1).and_then(Token::punct) != Some('.')
        {
            continue;
        }
        let Some(call) = tokens.get(start + 2).and_then(Token::ident) else {
            continue;
        };
        if !calls.contains(&call) || tokens.get(start + 3).and_then(Token::punct) != Some('(') {
            continue;
        }

        let mut paren_depth = 1usize;
        let mut brace_depth = 0usize;
        let mut block_depth = 0usize;
        let mut arg_index = 0usize;
        let mut cursor = start + 4;
        while cursor < tokens.len() {
            let token = &tokens[cursor];
            // A function/if/loop body inside an argument value is not a table constructor:
            // its `local name = value` statements must never be reported as table keys.
            if let Some(name) = token.ident() {
                match name {
                    "function" | "if" | "do" | "repeat" => block_depth += 1,
                    "end" | "until" => block_depth = block_depth.saturating_sub(1),
                    _ => {}
                }
            }
            let key_scope = paren_depth == 1 && brace_depth == 1 && block_depth == 0;
            match token.kind {
                TokenKind::Punct('(') => paren_depth += 1,
                TokenKind::Punct(')') => {
                    paren_depth -= 1;
                    if paren_depth == 0 {
                        break;
                    }
                }
                TokenKind::Punct('{') => brace_depth += 1,
                TokenKind::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
                TokenKind::Punct(',') if paren_depth == 1 && brace_depth == 0 => arg_index += 1,
                TokenKind::Ident(ref name) if key_scope => {
                    let is_assignment = tokens
                        .get(cursor + 1)
                        .and_then(Token::punct)
                        .is_some_and(|ch| ch == '=');
                    if is_assignment
                        && !tokens
                            .get(cursor + 2)
                            .and_then(Token::punct)
                            .is_some_and(|ch| ch == '=')
                    {
                        keys.push(TableKey {
                            call: call.to_owned(),
                            arg_index,
                            key: name.clone(),
                            line: token.line,
                        });
                    }
                }
                TokenKind::Punct('[') if key_scope => {
                    // `["bracketed key"] = value`
                    if let Some(Token {
                        kind: TokenKind::Str(name),
                        line,
                    }) = tokens.get(cursor + 1)
                        && tokens.get(cursor + 2).and_then(Token::punct) == Some(']')
                        && tokens.get(cursor + 3).and_then(Token::punct) == Some('=')
                    {
                        keys.push(TableKey {
                            call: call.to_owned(),
                            arg_index,
                            key: name.clone(),
                            line: *line,
                        });
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_strings_do_not_produce_keys_or_requires() {
        let source = r#"
            -- koma.monitor({ widht = 10 })
            --[[
              koma.workspace({ bogus = 1 })
              require("ghost")
            ]]
            koma.env("NOTE", "koma.monitor({ also_bogus = 2 })")
        "#;
        let tokens = tokenize(source);
        assert!(table_keys_for_calls(&tokens, &["monitor", "workspace"]).is_empty());
        assert!(require_targets(&tokens).is_empty());
    }

    #[test]
    fn long_strings_and_bracket_keys_are_tokenized() {
        let tokens = tokenize("local s = [==[ koma.bind ]==]\nkoma.workspace({ [\"na me\"] = 1 })");
        let keys = table_keys_for_calls(&tokens, &["workspace"]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, "na me");
    }

    #[test]
    fn nested_tables_and_function_arguments_are_not_outer_keys() {
        let source = r#"
            koma.appearance({ border = { widht = 2 }, animations = { enabled = true } })
            koma.bind("Mod + X", function(ctx)
                local opts = { bogus = 1 }
                ctx:spawn({ "foot" })
            end, { desc = "Spawn", cooldown_ms = 40 })
        "#;
        let tokens = tokenize(source);
        let keys = table_keys_for_calls(&tokens, &["appearance", "bind"]);
        let names: Vec<(&str, usize)> =
            keys.iter().map(|k| (k.key.as_str(), k.arg_index)).collect();
        assert_eq!(
            names,
            vec![
                ("border", 0),
                ("animations", 0),
                ("desc", 2),
                ("cooldown_ms", 2)
            ]
        );
    }

    #[test]
    fn require_forms_and_lines_are_reported() {
        let tokens = tokenize("require(\"macros.wps\")\nlocal x = require 'helpers'\n");
        let targets = require_targets(&tokens);
        assert_eq!(
            targets,
            vec![
                RequireTarget {
                    module: "macros.wps".to_owned(),
                    line: 1
                },
                RequireTarget {
                    module: "helpers".to_owned(),
                    line: 2
                },
            ]
        );
    }

    #[test]
    fn equality_comparison_is_not_a_table_key() {
        let tokens = tokenize("koma.gesture({ action = x == 1 and y or z, fingers = 3 })");
        let keys = table_keys_for_calls(&tokens, &["gesture"]);
        let names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(names, vec!["action", "fingers"]);
    }
}
