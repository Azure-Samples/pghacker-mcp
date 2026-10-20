// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! AGE's strings, backtick names and comments are not SQL's. This is a
//! known-write-clause check, not a complete Cypher parser.

use super::check_function;

#[derive(Debug)]
enum Token {
    Word(String),
    Quoted(String),
    Literal,
    Punct(char),
}

pub(super) fn check(text: &str) -> Result<(), String> {
    let tokens = tokenize(text)?;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') => depth = depth.saturating_sub(1),
            Token::Word(word) | Token::Quoted(word) => {
                if matches!(tokens.get(index + 1), Some(Token::Punct('('))) {
                    let name = if matches!(token, Token::Word(_)) {
                        word.to_ascii_lowercase()
                    } else {
                        word.clone()
                    };
                    check_function(&name)?;
                    if !matches!(
                        index.checked_sub(1).and_then(|i| tokens.get(i)),
                        Some(Token::Punct('.'))
                    ) {
                        // AGE also resolves unqualified names to lower-case age_<name> functions.
                        check_function(&format!("age_{}", word.to_ascii_lowercase()))?;
                    }
                }
                if depth != 0 || !matches!(token, Token::Word(_)) {
                    continue;
                }
                let upper = word.to_ascii_uppercase();
                if matches!(
                    upper.as_str(),
                    "CREATE"
                        | "MERGE"
                        | "SET"
                        | "DELETE"
                        | "DETACH"
                        | "REMOVE"
                        | "CALL"
                        | "LOAD"
                        | "FOREACH"
                ) && !expression_position(&tokens, index)
                {
                    return Err(format!(
                        "query only reads: this Cypher query has a {upper} clause. \
                         Use MATCH/RETURN without graph updates, procedures or loading"
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn expression_position(tokens: &[Token], index: usize) -> bool {
    let before = |offset: usize| index.checked_sub(offset).and_then(|i| tokens.get(i));
    match before(1) {
        // `WITH *` and `RETURN *` project every variable: what follows starts a clause.
        Some(Token::Punct('*'))
            if matches!(before(2), Some(Token::Word(word))
                if matches!(word.to_ascii_uppercase().as_str(), "WITH" | "RETURN" | "DISTINCT")) =>
        {
            false
        }
        Some(Token::Punct('.' | ':' | ',' | '=' | '+' | '-' | '*' | '/' | '<' | '>' | '|')) => true,
        Some(Token::Word(word)) => matches!(
            word.to_ascii_uppercase().as_str(),
            "RETURN"
                | "WITH"
                | "WHERE"
                | "UNWIND"
                | "DISTINCT"
                | "AS"
                | "AND"
                | "OR"
                | "NOT"
                | "XOR"
                | "THEN"
                | "ELSE"
                | "WHEN"
                | "BY"
                | "IN"
                | "IS"
                | "CASE"
        ),
        _ => false,
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let invalid = |detail: &str| format!("query can't read the Cypher: {detail}");
    let mut chars = text.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' => {}
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                for ch in chars.by_ref() {
                    if matches!(ch, '\n' | '\r') {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut closed = false;
                // AGE block comments end at the first */; they do not nest like SQL comments.
                while let Some(ch) = chars.next() {
                    if ch == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(invalid("unterminated comment"));
                }
            }
            quote @ ('\'' | '"' | '`') => {
                let mut value = String::new();
                loop {
                    let Some(ch) = chars.next() else {
                        return Err(invalid("unterminated string or backtick identifier"));
                    };
                    if ch == quote {
                        if chars.peek() == Some(&quote) {
                            chars.next();
                            value.push(quote);
                        } else {
                            break;
                        }
                    } else if ch == '\\' && quote != '`' {
                        let escaped = chars.next().ok_or_else(|| invalid("incomplete escape"))?;
                        value.push(escaped);
                    } else {
                        value.push(ch);
                    }
                }
                tokens.push(if quote == '`' {
                    Token::Quoted(value)
                } else {
                    Token::Literal
                });
            }
            '$' if chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || *ch == '_' || !ch.is_ascii()) =>
            {
                while chars.peek().is_some_and(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$') || !ch.is_ascii()
                }) {
                    chars.next();
                }
                tokens.push(Token::Literal);
            }
            ch if ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii() => {
                let mut word = String::from(ch);
                while chars.peek().is_some_and(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$') || !ch.is_ascii()
                }) {
                    word.push(chars.next().expect("peeked character"));
                }
                tokens.push(Token::Word(word));
            }
            ch if ch.is_ascii_digit() => {
                while chars
                    .peek()
                    .is_some_and(|ch| ch.is_ascii_digit() || *ch == '.')
                {
                    chars.next();
                }
                tokens.push(Token::Literal);
            }
            ch => tokens.push(Token::Punct(ch)),
        }
    }
    Ok(tokens)
}
