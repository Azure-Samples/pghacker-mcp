// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! The query boundary: recursive checks of parsed reads and token-aware
//! checks of known unsafe forms in the retained PostgreSQL/AGE fallback.

use std::borrow::Cow;
use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName, Query, Select, Set,
    SetExpr, Statement, TableFactor, Value, Visit, Visitor,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

mod cypher;

/// How a statement passed the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// sqlparser parsed it, and the walk of its syntax tree found only reads.
    Parsed,
    /// The checks of its tokens passed it: sqlparser couldn't parse it, or it
    /// nests deeper than the walk goes.
    Tokens(Fallback),
}

/// Why a statement was checked by its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    /// sqlparser couldn't parse it: its error, which quotes the statement.
    Unparsed(String),
    /// It nests deeper than the walk's 64 levels.
    TooDeep,
}

impl Fallback {
    /// Its name in usage events: `unparsed` or `too_deep`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unparsed(_) => "unparsed",
            Self::TooDeep => "too_deep",
        }
    }
}

pub(super) fn check(sql: &str, max_bytes: usize) -> Result<Checked, String> {
    if sql.len() > max_bytes {
        return Err(format!(
            "the SQL is {} bytes; query takes at most {max_bytes}",
            sql.len()
        ));
    }
    let tokens = tokens(sql)?;
    let statements: Vec<_> = tokens
        .split(|token| *token == Token::SemiColon)
        .filter(|statement| !statement.is_empty())
        .collect();
    let statement = match statements.as_slice() {
        [] => return Err("the SQL has no statement".to_owned()),
        [statement] => *statement,
        many => {
            return Err(format!(
                "the SQL has {} statements; query runs one at a time",
                many.len()
            ));
        }
    };
    check_head(statement)?;
    match Parser::new(&PostgreSqlDialect {})
        .with_tokens(statement.to_vec())
        .parse_statements()
    {
        Ok(parsed) => match parsed.visit(&mut ReadOnly::default()) {
            ControlFlow::Continue(()) => Ok(Checked::Parsed),
            ControlFlow::Break(Stop::Refused(reason)) => Err(reason),
            ControlFlow::Break(Stop::TokenFallback) => {
                check_fallback(statement).map(|()| Checked::Tokens(Fallback::TooDeep))
            }
        },
        Err(error) => check_fallback(statement)
            .map(|()| Checked::Tokens(Fallback::Unparsed(error.to_string()))),
    }
}

fn not_a_read(name: &str) -> String {
    format!(
        "query only reads: it takes one SELECT, WITH, VALUES, TABLE, EXPLAIN or SHOW statement, \
         not {name}"
    )
}

fn transaction_refusal(name: &str) -> String {
    format!(
        "query runs each statement in a read-only transaction of its own, which it rolls back, \
         so it doesn't take {name}"
    )
}

fn settings_refusal(name: &str) -> String {
    format!(
        "{}: changing session settings, system settings or identity is not allowed. \
         Use SHOW name or current_setting('name') to inspect settings",
        not_a_read(name)
    )
}

fn into_refusal() -> String {
    "query only reads: SELECT INTO creates a table. Return the SELECT result directly, \
     or use a read-only WITH query instead"
        .to_owned()
}

fn lock_refusal(clause: &str) -> String {
    format!("query only reads: {clause} locks rows. Remove the locking clause and retry")
}

fn show_refusal() -> String {
    "query only reads: SHOW takes a PostgreSQL setting or ALL, not another dialect's SHOW \
     command. Use a PostgreSQL catalog SELECT for metadata"
        .to_owned()
}

fn keyword(token: Option<&Token>, expected: Keyword) -> bool {
    matches!(token, Some(Token::Word(word)) if word.quote_style.is_none() && word.keyword == expected)
}

fn check_head(mut tokens: &[Token]) -> Result<(), String> {
    loop {
        while tokens.first() == Some(&Token::LParen) {
            tokens = &tokens[1..];
        }
        let Some(first) = tokens.first() else {
            return Ok(());
        };
        let Token::Word(word) = first else {
            return Err(not_a_read(&first.to_string()));
        };
        if word.quote_style.is_some() {
            return Err(not_a_read(&word.to_string()));
        }
        match word.keyword {
            Keyword::SELECT | Keyword::WITH | Keyword::VALUES | Keyword::TABLE => return Ok(()),
            Keyword::SHOW => {
                let qualified_setting = tokens.get(2) == Some(&Token::Period);
                let session_option = keyword(tokens.get(1), Keyword::SESSION)
                    && !keyword(tokens.get(2), Keyword::AUTHORIZATION);
                if !qualified_setting && (session_option || tokens.get(1).is_some_and(|token| {
                    matches!(token, Token::Word(word) if word.quote_style.is_none()
                        && matches!(word.value.to_ascii_uppercase().as_str(),
                            "TERSE" | "EXTENDED" | "FULL" | "GLOBAL" | "LOCAL" | "EXTERNAL" | "COLUMNS"
                            | "FIELDS" | "TABLES" | "VIEWS" | "MATERIALIZED" | "FUNCTIONS"
                            | "PROCESSLIST" | "CREATE" | "COLLATION" | "VARIABLES" | "STATUS"
                            | "CATALOGS" | "DATABASES" | "SCHEMAS" | "CHARACTER" | "CHARSET"))
                })) {
                    return Err(show_refusal());
                }
                return Ok(());
            }
            Keyword::EXPLAIN => {
                tokens = &tokens[1..];
                if tokens.first() == Some(&Token::LParen) {
                    let Some(end) = closing_paren(tokens, 0) else {
                        return Ok(());
                    };
                    tokens = &tokens[end + 1..];
                }
                while tokens.first().is_some_and(|token| {
                    matches!(token, Token::Word(word) if word.quote_style.is_none()
                        && matches!(word.value.to_ascii_uppercase().as_str(), "ANALYZE" | "ANALYSE" | "VERBOSE"))
                }) {
                    tokens = &tokens[1..];
                }
            }
            Keyword::BEGIN | Keyword::START => {
                return Err(transaction_refusal("BEGIN or START TRANSACTION"));
            }
            Keyword::COMMIT | Keyword::END => {
                return Err(transaction_refusal("COMMIT or END"));
            }
            Keyword::ROLLBACK | Keyword::ABORT => {
                return Err(transaction_refusal(
                    "ROLLBACK, ABORT or ROLLBACK TO SAVEPOINT",
                ));
            }
            Keyword::SAVEPOINT => return Err(transaction_refusal("SAVEPOINT")),
            Keyword::RELEASE => return Err(transaction_refusal("RELEASE SAVEPOINT")),
            Keyword::SET => {
                if keyword(tokens.get(1), Keyword::TRANSACTION)
                    || (keyword(tokens.get(1), Keyword::SESSION)
                        && keyword(tokens.get(2), Keyword::CHARACTERISTICS))
                {
                    return Err(transaction_refusal(
                        "SET TRANSACTION or SET SESSION CHARACTERISTICS AS TRANSACTION",
                    ));
                }
                return Err(settings_refusal("SET"));
            }
            Keyword::RESET => return Err(settings_refusal("RESET")),
            Keyword::ALTER if keyword(tokens.get(1), Keyword::SYSTEM) => {
                return Err(settings_refusal("ALTER SYSTEM"));
            }
            _ => return Err(not_a_read(&word.value.to_uppercase())),
        }
    }
}

const MAX_VISIT_DEPTH: usize = 64;

enum Stop {
    Refused(String),
    TokenFallback,
}

#[derive(Default)]
struct ReadOnly {
    depth: usize,
}

impl ReadOnly {
    fn enter(&mut self) -> ControlFlow<Stop> {
        if self.depth >= MAX_VISIT_DEPTH {
            return ControlFlow::Break(Stop::TokenFallback);
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn leave(&mut self) -> ControlFlow<Stop> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
}

fn flow(result: Result<(), String>) -> ControlFlow<Stop> {
    match result {
        Ok(()) => ControlFlow::Continue(()),
        Err(reason) => ControlFlow::Break(Stop::Refused(reason)),
    }
}

impl Visitor for ReadOnly {
    type Break = Stop;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Stop> {
        self.enter()?;
        flow(match statement {
            Statement::Query(_) | Statement::ShowVariable { .. } | Statement::Explain { .. } => {
                Ok(())
            }
            Statement::Set(Set::SetTransaction { .. }) => Err(transaction_refusal(
                "SET TRANSACTION or SET SESSION CHARACTERISTICS AS TRANSACTION",
            )),
            Statement::Set(_) => Err(settings_refusal("SET")),
            Statement::Reset(_) => Err(settings_refusal("RESET")),
            Statement::Insert(_) => Err(not_a_read("INSERT")),
            Statement::Update(_) => Err(not_a_read("UPDATE")),
            Statement::Delete(_) => Err(not_a_read("DELETE")),
            Statement::Merge(_) => Err(not_a_read("MERGE")),
            _ => Err(not_a_read("that statement")),
        })
    }

    fn post_visit_statement(&mut self, _: &Statement) -> ControlFlow<Stop> {
        self.leave()
    }

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Stop> {
        self.enter()?;
        if let Some(lock) = query.locks.first() {
            return flow(Err(lock_refusal(&lock.to_string())));
        }
        if query.settings.is_some() {
            return flow(Err(settings_refusal("SETTINGS")));
        }
        // Set-operation nodes have no visitor hook, so bound their recursion separately.
        let mut bodies = vec![(query.body.as_ref(), 0)];
        while let Some((body, depth)) = bodies.pop() {
            if depth >= MAX_VISIT_DEPTH {
                return ControlFlow::Break(Stop::TokenFallback);
            }
            if let SetExpr::SetOperation { left, right, .. } = body {
                bodies.push((left.as_ref(), depth + 1));
                bodies.push((right.as_ref(), depth + 1));
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<Stop> {
        self.leave()
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Stop> {
        self.enter()?;
        if select.into.is_some() {
            return flow(Err(into_refusal()));
        }
        ControlFlow::Continue(())
    }

    fn post_visit_select(&mut self, _: &Select) -> ControlFlow<Stop> {
        self.leave()
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Stop> {
        self.enter()?;
        if let Expr::Function(function) = expr {
            let args = match &function.args {
                FunctionArguments::List(args) => args.args.as_slice(),
                _ => &[],
            };
            return flow(check_call(&function.name, args));
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, _: &Expr) -> ControlFlow<Stop> {
        self.leave()
    }

    fn pre_visit_table_factor(&mut self, table: &TableFactor) -> ControlFlow<Stop> {
        self.enter()?;
        match table {
            TableFactor::Table {
                name,
                args: Some(args),
                ..
            } => flow(check_call(name, &args.args)),
            TableFactor::Function { name, args, .. } => flow(check_call(name, args)),
            _ => ControlFlow::Continue(()),
        }
    }

    fn post_visit_table_factor(&mut self, _: &TableFactor) -> ControlFlow<Stop> {
        self.leave()
    }
}

fn identifier(value: &str, quoted: bool) -> Cow<'_, str> {
    if quoted {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(value.to_ascii_lowercase())
    }
}

fn check_call(name: &ObjectName, args: &[FunctionArg]) -> Result<(), String> {
    let Some(Ident {
        value, quote_style, ..
    }) = name.0.last().and_then(|part| part.as_ident())
    else {
        return Ok(());
    };
    let name = identifier(value, quote_style.is_some());
    check_function(&name)?;
    if name == "cypher" {
        let text = match args.get(1) {
            Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))) => literal(expr),
            _ => None,
        };
        cypher::check(text.ok_or_else(cypher_literal_refusal)?)?;
    }
    Ok(())
}

fn literal(mut expr: &Expr) -> Option<&str> {
    loop {
        match expr {
            Expr::Nested(inner) | Expr::Cast { expr: inner, .. } => expr = inner,
            Expr::Value(value) => {
                return match &value.value {
                    Value::SingleQuotedString(text)
                    | Value::EscapedStringLiteral(text)
                    | Value::UnicodeStringLiteral(text) => Some(text),
                    Value::DollarQuotedString(text) => Some(&text.value),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
}

fn cypher_literal_refusal() -> String {
    "query only reads: cypher() needs literal query text as its second argument so it can \
     check for graph updates. Pass a SQL string or $$ MATCH ... RETURN ... $$, not computed text"
        .to_owned()
}

pub(super) fn check_function(name: &str) -> Result<(), String> {
    let (effect, hint) = match name {
        "set_config" | "setseed" | "set_limit" => (
            "changes session settings",
            "Use SHOW name or current_setting('name') to inspect settings",
        ),
        name if name.starts_with("pg_advisory_") || name.starts_with("pg_try_advisory_") => (
            "changes advisory locks",
            "Read without advisory coordination",
        ),
        "pg_sleep" | "pg_sleep_for" | "pg_sleep_until" => (
            "deliberately delays the call",
            "Run the read without a sleep",
        ),
        "pg_notify" => ("sends notifications", "Read without notifications"),
        "graph_request_full" | "restore_addresses" | "rethread_mail" => (
            "changes corpus maintenance state",
            "Query the corpus tables without scheduling repair or sync jobs",
        ),
        "pg_export_snapshot" => (
            "exports session coordination state",
            "Run the read without exporting snapshots",
        ),
        "nextval" | "setval" => (
            "changes a sequence",
            "Read without advancing or setting sequences",
        ),
        "pg_cancel_backend"
        | "pg_terminate_backend"
        | "pg_reload_conf"
        | "pg_rotate_logfile"
        | "pg_log_backend_memory_contexts"
        | "pg_log_standby_snapshot"
        | "pg_import_system_collations"
        | "pg_extension_config_dump"
        | "pg_nextoid"
        | "pg_stop_making_pinned_objects" => (
            "controls the server or its processes",
            "Use read-only catalog inspection instead",
        ),
        name if name.starts_with("binary_upgrade_") => (
            "controls server upgrade state",
            "Use read-only catalog inspection instead",
        ),
        name if name.starts_with("bm25_compact")
            || name.starts_with("bm25_test_")
            || matches!(
                name,
                "brin_desummarize_range"
                    | "brin_summarize_new_values"
                    | "brin_summarize_range"
                    | "gin_clean_pending_list"
                    | "bm25_cache_apply_to_tail"
                    | "bm25_cache_bump_spill_generation"
                    | "bm25_cache_cold_build"
                    | "bm25_cache_evict_largest"
                    | "bm25_force_merge"
                    | "bm25_spill_index"
            ) =>
        {
            (
                "maintains or tests index state",
                "Use ordinary index-backed SELECT queries instead",
            )
        }
        "pg_switch_wal"
        | "pg_switch_xlog"
        | "pg_backup_start"
        | "pg_backup_stop"
        | "pg_start_backup"
        | "pg_stop_backup"
        | "pg_create_restore_point"
        | "pg_promote"
        | "pg_wal_replay_pause"
        | "pg_wal_replay_resume"
        | "pg_replication_slot_advance"
        | "pg_create_physical_replication_slot"
        | "pg_create_logical_replication_slot"
        | "pg_drop_replication_slot"
        | "pg_copy_physical_replication_slot"
        | "pg_copy_logical_replication_slot"
        | "pg_sync_replication_slots"
        | "pg_logical_slot_get_changes"
        | "pg_logical_slot_get_binary_changes"
        | "pg_logical_emit_message"
        | "pg_replication_origin_create"
        | "pg_replication_origin_drop"
        | "pg_replication_origin_advance"
        | "pg_replication_origin_session_setup"
        | "pg_replication_origin_session_reset"
        | "pg_replication_origin_xact_setup"
        | "pg_replication_origin_xact_reset" => (
            "changes WAL, backup or replication state",
            "Use read-only WAL or replication inspection instead",
        ),
        name if name.starts_with("pg_stat_reset")
            || matches!(name, "pg_stat_clear_snapshot" | "pg_stat_force_next_flush") =>
        {
            (
                "changes statistics state",
                "Read pg_stat views without resetting statistics",
            )
        }
        name if name.starts_with("pg_ls_")
            || name.starts_with("lo_")
            || matches!(
                name,
                "pg_read_file"
                    | "pg_read_binary_file"
                    | "pg_stat_file"
                    | "pg_file_write"
                    | "pg_file_rename"
                    | "pg_file_unlink"
                    | "pg_file_sync"
                    | "loread"
                    | "lowrite"
                    | "loopen"
                    | "loclose"
            ) =>
        {
            (
                "accesses server files or large objects",
                "Query the corpus tables instead",
            )
        }
        name if name.starts_with("dblink") || name.starts_with("postgres_fdw_") => (
            "uses external database connections",
            "Query the local corpus tables instead",
        ),
        name if name.starts_with("query_to_xml") || matches!(name, "ts_stat" | "ts_rewrite") => (
            "can execute SQL supplied as text",
            "Express the read directly as SQL instead",
        ),
        "create_graph"
        | "alter_graph"
        | "drop_graph"
        | "create_vlabel"
        | "create_elabel"
        | "drop_label"
        | "load_labels_from_file"
        | "load_edges_from_file"
        | "age_create_barbell_graph"
        | "create_complete_graph"
        | "_cypher_create_clause"
        | "_cypher_delete_clause"
        | "_cypher_merge_clause"
        | "_cypher_set_clause" => (
            "changes or loads the AGE graph",
            "Use cypher() with MATCH/RETURN without graph updates",
        ),
        _ => return Ok(()),
    };
    Err(format!("query only reads: {name}() {effect}. {hint}"))
}

fn closing_paren(tokens: &[Token], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn check_fallback(tokens: &[Token]) -> Result<(), String> {
    let cte_names = check_ctes(tokens)?;
    for (index, token) in tokens.iter().enumerate() {
        let qualified = index > 0 && tokens[index - 1] == Token::Period;
        if !qualified && keyword(Some(token), Keyword::INTO) {
            return Err(into_refusal());
        } else if !qualified && keyword(Some(token), Keyword::FOR) {
            let rest = &tokens[index + 1..];
            let end = if keyword(rest.first(), Keyword::NO) && keyword(rest.get(1), Keyword::KEY) {
                2
            } else if keyword(rest.first(), Keyword::KEY) {
                1
            } else {
                0
            };
            if keyword(rest.get(end), Keyword::UPDATE) || keyword(rest.get(end), Keyword::SHARE) {
                let clause = tokens[index..=index + end + 1]
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                return Err(lock_refusal(&clause));
            }
        }
        let Token::Word(word) = token else {
            continue;
        };
        if tokens.get(index + 1) != Some(&Token::LParen)
            || cte_names.contains(&index)
            || is_alias_or_type(tokens, index)
        {
            continue;
        }
        let name = identifier(&word.value, word.quote_style.is_some());
        check_function(&name)?;
        if name == "cypher" {
            let args = call_arguments(tokens, index + 1).ok_or_else(cypher_literal_refusal)?;
            let query = args.get(1).ok_or_else(cypher_literal_refusal)?;
            let mut parser = Parser::new(&PostgreSqlDialect {}).with_tokens(query.to_vec());
            let expr = parser.parse_expr().map_err(|_| cypher_literal_refusal())?;
            if parser.peek_token().token != Token::EOF {
                return Err(cypher_literal_refusal());
            }
            cypher::check(literal(&expr).ok_or_else(cypher_literal_refusal)?)?;
        }
    }
    Ok(())
}

fn check_ctes(tokens: &[Token]) -> Result<Vec<usize>, String> {
    let mut names = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if !matches!(token, Token::Word(_)) {
            continue;
        }
        let mut next = index + 1;
        if tokens.get(next) == Some(&Token::LParen) {
            next += 1;
            while matches!(tokens.get(next), Some(Token::Word(_) | Token::Comma)) {
                next += 1;
            }
            if tokens.get(next) != Some(&Token::RParen) {
                continue;
            }
            next += 1;
        }
        if !keyword(tokens.get(next), Keyword::AS) {
            continue;
        }
        next += 1;
        if keyword(tokens.get(next), Keyword::NOT) {
            next += 1;
        }
        if keyword(tokens.get(next), Keyword::MATERIALIZED) {
            next += 1;
        }
        if tokens.get(next) != Some(&Token::LParen) {
            continue;
        }
        let mut body = &tokens[next + 1..];
        while body.first() == Some(&Token::LParen) {
            body = &body[1..];
        }
        if matches!(body.first(), Some(Token::Word(word)) if word.quote_style.is_none()
            && matches!(word.keyword,
                Keyword::SELECT | Keyword::WITH | Keyword::VALUES | Keyword::TABLE
                | Keyword::INSERT | Keyword::UPDATE | Keyword::DELETE | Keyword::MERGE))
        {
            check_head(body)?;
            names.push(index);
        }
    }
    Ok(names)
}

fn is_alias_or_type(tokens: &[Token], index: usize) -> bool {
    let mut start = index;
    while start >= 2
        && tokens[start - 1] == Token::Period
        && matches!(tokens[start - 2], Token::Word(_))
    {
        start -= 2;
    }
    let before = start.checked_sub(1).and_then(|i| tokens.get(i));
    if keyword(before, Keyword::AS) || matches!(before, Some(Token::DoubleColon | Token::RParen)) {
        return true;
    }
    // An implicit table alias with a column list, including AGE record types.
    let Some(mut relation) = start.checked_sub(1) else {
        return false;
    };
    if !matches!(tokens[relation], Token::Word(_)) {
        return false;
    }
    while relation >= 2
        && tokens[relation - 1] == Token::Period
        && matches!(tokens[relation - 2], Token::Word(_))
    {
        relation -= 2;
    }
    let before_relation = relation.checked_sub(1).and_then(|i| tokens.get(i));
    keyword(before_relation, Keyword::FROM) || keyword(before_relation, Keyword::JOIN)
}

fn call_arguments(tokens: &[Token], start: usize) -> Option<Vec<&[Token]>> {
    let end = closing_paren(tokens, start)?;
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut begin = start + 1;
    for index in start + 1..end {
        match tokens[index] {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth = depth.checked_sub(1)?,
            Token::Comma if depth == 0 => {
                args.push(&tokens[begin..index]);
                begin = index + 1;
            }
            _ => {}
        }
    }
    args.push(&tokens[begin..end]);
    Some(args)
}

fn tokens(sql: &str) -> Result<Vec<Token>, String> {
    let raw = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .tokenize()
        .map_err(|error| format!("query can't read the SQL: {error}"))?;
    let mut result = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        // sqlparser handles Unicode strings, but not PostgreSQL's U&"identifier".
        if matches!(&raw[index], Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case("u"))
            && raw.get(index + 1) == Some(&Token::Ampersand)
            && let Some(Token::Word(word)) = raw.get(index + 2)
            && word.quote_style == Some('"')
        {
            let mut end = index + 3;
            while matches!(raw.get(end), Some(Token::Whitespace(_))) {
                end += 1;
            }
            let mut escape = '\\';
            if keyword(raw.get(end), Keyword::UESCAPE) {
                end += 1;
                while matches!(raw.get(end), Some(Token::Whitespace(_))) {
                    end += 1;
                }
                let value = match raw.get(end) {
                    Some(
                        Token::SingleQuotedString(value)
                        | Token::EscapedStringLiteral(value)
                        | Token::UnicodeStringLiteral(value),
                    ) => value.as_str(),
                    Some(Token::DollarQuotedString(value)) => value.value.as_str(),
                    _ => {
                        return Err(
                            "query can't read the SQL: invalid UESCAPE character".to_owned()
                        );
                    }
                };
                let mut chars = value.chars();
                escape = chars
                    .next()
                    .ok_or("query can't read the SQL: empty UESCAPE")?;
                if chars.next().is_some()
                    || escape.is_ascii_hexdigit()
                    || escape.is_whitespace()
                    || matches!(escape, '+' | '\'' | '"')
                {
                    return Err("query can't read the SQL: invalid UESCAPE character".to_owned());
                }
                end += 1;
            }
            result.push(Token::make_word(
                &unicode_identifier(&word.value, escape)?,
                Some('"'),
            ));
            index = end;
        } else {
            if !matches!(raw[index], Token::Whitespace(_)) {
                result.push(raw[index].clone());
            }
            index += 1;
        }
    }
    Ok(result)
}

fn unicode_identifier(text: &str, escape: char) -> Result<String, String> {
    let invalid = || "query can't read the SQL: invalid Unicode identifier escape".to_owned();
    let mut chars = text.chars().peekable();
    let mut units = Vec::new();
    while let Some(ch) = chars.next() {
        if ch != escape || chars.peek() == Some(&escape) {
            if ch == escape {
                chars.next();
            }
            units.extend_from_slice(ch.encode_utf16(&mut [0; 2]));
            continue;
        }
        let count = if chars.peek() == Some(&'+') {
            chars.next();
            6
        } else {
            4
        };
        let digits: String = chars.by_ref().take(count).collect();
        if digits.len() != count || !digits.bytes().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let code = u32::from_str_radix(&digits, 16).map_err(|_| invalid())?;
        if code <= u32::from(u16::MAX) {
            units.push(code as u16);
        } else {
            units.extend_from_slice(
                char::from_u32(code)
                    .ok_or_else(invalid)?
                    .encode_utf16(&mut [0; 2]),
            );
        }
    }
    String::from_utf16(&units).map_err(|_| invalid())
}
