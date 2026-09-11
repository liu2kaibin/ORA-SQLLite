use std::collections::{BTreeSet, HashSet};

use sqlparser::dialect::OracleDialect;
pub(super) use sqlparser::tokenizer::TokenWithSpan;
use sqlparser::tokenizer::{Span, Token, Tokenizer};

use super::source::SourceMap;
use crate::sql::{ParameterKey, ParameterStyle, SqlLimits, SqlSpan};
use crate::{DbError, ErrorCode, Result};

type ParameterOccurrence = (ParameterKey, SqlSpan);

pub(super) fn tokenize(sql: &str, limits: &SqlLimits) -> Result<Vec<TokenWithSpan>> {
    if sql.len() > limits.max_sql_bytes() as usize {
        return Err(DbError::new(
            ErrorCode::Syntax,
            "SQL text exceeds the configured byte limit",
        ));
    }

    let source_map = SourceMap::new(sql)?;
    let raw_tokens = Tokenizer::new(&OracleDialect, sql)
        .tokenize_with_location()
        .map_err(|error| {
            source_map
                .sqlparser_location(error.location)
                .map(|location| {
                    DbError::new(ErrorCode::Syntax, error.message).with_sql_location(location)
                })
                .unwrap_or_else(|mapping_error| mapping_error)
        })?;
    let (tokens, parameters) =
        rewrite_oracle_binds(sql, &source_map, raw_tokens, limits.max_tokens() as usize)?;

    validate_parameter_sequence(&parameters, limits.max_parameters() as usize, &source_map)?;
    Ok(tokens)
}

fn rewrite_oracle_binds(
    sql: &str,
    source_map: &SourceMap<'_>,
    raw_tokens: Vec<TokenWithSpan>,
    max_tokens: usize,
) -> Result<(Vec<TokenWithSpan>, Vec<ParameterOccurrence>)> {
    let mut tokens = Vec::with_capacity(raw_tokens.len());
    let mut parameters = Vec::new();
    let mut token_count = 0;
    let mut index = 0;

    while index < raw_tokens.len() {
        let token = &raw_tokens[index];
        if token.token != Token::Colon {
            charge_token(&token.token, &mut token_count, max_tokens)?;
            tokens.push(token.clone());
            index += 1;
            continue;
        }

        let colon_span = source_map.span(token.span)?;
        let mut candidate_end = colon_span.end();
        let mut next_index = index + 1;
        while let Some(next) = raw_tokens.get(next_index) {
            let next_span = source_map.span(next.span)?;
            if next_span.start() != candidate_end
                || !is_bind_body_token(sql, next_span, &next.token)
            {
                break;
            }
            candidate_end = next_span.end();
            next_index += 1;
        }

        if next_index == index + 1 {
            if let Some(next) = raw_tokens.get(next_index) {
                let next_span = source_map.span(next.span)?;
                if next_span.start() == colon_span.end()
                    && matches!(&next.token, Token::Word(word) if word.quote_style.is_some())
                {
                    return Err(DbError::new(
                        ErrorCode::Bind,
                        "parameter name must be an unquoted identifier",
                    )
                    .with_sql_location(source_map.location(colon_span.start())?));
                }
            }
            charge_token(&token.token, &mut token_count, max_tokens)?;
            tokens.push(token.clone());
            index += 1;
            continue;
        }

        let span = SqlSpan::new(colon_span.start(), candidate_end)?;
        let text = &sql[span.start()..span.end()];
        charge_non_whitespace(&mut token_count, max_tokens)?;
        let parameter = ParameterKey::parse(text, span)?;
        let upstream_span = Span::new(token.span.start, raw_tokens[next_index - 1].span.end);
        tokens.push(TokenWithSpan::new(
            Token::Placeholder(text.to_owned()),
            upstream_span,
        ));
        parameters.push((parameter, span));
        index = next_index;
    }

    Ok((tokens, parameters))
}

fn is_bind_body_token(sql: &str, span: SqlSpan, token: &Token) -> bool {
    match token {
        Token::Number(_, _) => true,
        Token::Word(word) => word.quote_style.is_none(),
        Token::Whitespace(_) => false,
        _ => sql[span.start()..span.end()]
            .chars()
            .all(is_owned_bind_character),
    }
}

fn is_owned_bind_character(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '_' | '$' | '#')
        || !character.is_ascii()
}

fn charge_token(token: &Token, count: &mut usize, max_tokens: usize) -> Result<()> {
    if matches!(token, Token::Whitespace(_)) {
        return Ok(());
    }
    charge_non_whitespace(count, max_tokens)
}

fn charge_non_whitespace(count: &mut usize, max_tokens: usize) -> Result<()> {
    *count = count.checked_add(1).ok_or_else(token_limit_error)?;
    if *count > max_tokens {
        return Err(token_limit_error());
    }
    Ok(())
}

fn token_limit_error() -> DbError {
    DbError::new(
        ErrorCode::Syntax,
        "SQL text exceeds the configured token limit",
    )
}

fn validate_parameter_sequence(
    parameters: &[ParameterOccurrence],
    max_parameters: usize,
    source_map: &SourceMap<'_>,
) -> Result<()> {
    let Some(first) = parameters.first() else {
        return Ok(());
    };
    let style = first.0.style();
    if parameters
        .iter()
        .any(|(parameter, _)| parameter.style() != style)
    {
        return Err(DbError::new(
            ErrorCode::Bind,
            "named and positional parameters must not be mixed",
        ));
    }

    if style == ParameterStyle::Positional {
        let positions = parameters
            .iter()
            .filter_map(|(parameter, _)| match parameter {
                ParameterKey::Position(position) => Some(position.get()),
                ParameterKey::Name(_) => None,
            })
            .collect::<BTreeSet<_>>();
        if positions
            .iter()
            .copied()
            .enumerate()
            .any(|(index, position)| position as usize != index + 1)
        {
            return Err(DbError::new(
                ErrorCode::Bind,
                "positional parameters must be contiguous starting at :1",
            ));
        }
    }

    let mut distinct = HashSet::new();
    for (parameter, span) in parameters {
        if distinct.insert(parameter) && distinct.len() > max_parameters {
            return Err(DbError::new(
                ErrorCode::Bind,
                "SQL statement exceeds the configured logical parameter limit",
            )
            .with_sql_location(source_map.location(span.start())?));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlparser::tokenizer::{Location, Token};

    use super::{TokenWithSpan, tokenize};
    use crate::error::SqlLocation;
    use crate::{ErrorCode, sql::SqlLimits};

    fn placeholder_texts(tokens: &[TokenWithSpan]) -> Vec<&str> {
        tokens
            .iter()
            .filter_map(|token| match &token.token {
                Token::Placeholder(value) => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bind_rewrite_ignores_literals_comments_and_quoted_names() {
        let sql = "SELECT ':1', \"A:B\" FROM T -- :2\n WHERE ID = :1";
        let tokens = tokenize(sql, &SqlLimits::default()).unwrap();
        assert_eq!(placeholder_texts(&tokens), vec![":1"]);
    }

    #[test]
    fn token_limit_fails_before_ast_conversion() {
        let limits = SqlLimits::default().with_max_tokens(3).unwrap();
        assert_eq!(
            tokenize("SELECT A FROM T", &limits).unwrap_err().code(),
            ErrorCode::Syntax
        );
    }

    #[test]
    fn token_limit_stops_before_later_bind_validation() {
        let limits = SqlLimits::default().with_max_tokens(1).unwrap();
        assert_eq!(
            tokenize("SELECT A, :0", &limits).unwrap_err().code(),
            ErrorCode::Syntax
        );
    }

    #[test]
    fn sql_byte_limit_fails_before_tokenization() {
        let limits = SqlLimits::default().with_max_sql_bytes(2).unwrap();
        assert_eq!(
            tokenize("一", &limits).unwrap_err().code(),
            ErrorCode::Syntax
        );
    }

    #[test]
    fn tokenizer_errors_keep_original_byte_locations() {
        let error = tokenize("SELECT '", &SqlLimits::default()).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Syntax);
        assert_eq!(
            error.sql_location(),
            Some(SqlLocation::new(7, 1, 8).unwrap())
        );
    }

    #[test]
    fn exact_token_limit_ignores_whitespace_and_comments() {
        let limits = SqlLimits::default().with_max_tokens(4).unwrap();
        assert!(tokenize("SELECT A -- ignored\n FROM T", &limits).is_ok());
    }

    #[test]
    fn bind_rewrite_requires_source_adjacency() {
        let tokens = tokenize(
            "SELECT :first, : second, :\nthird FROM T",
            &SqlLimits::default(),
        )
        .unwrap();
        assert_eq!(placeholder_texts(&tokens), vec![":first"]);
    }

    #[test]
    fn rewritten_bind_keeps_its_original_source_span() {
        let tokens = tokenize("SELECT :name", &SqlLimits::default()).unwrap();
        let bind = tokens
            .iter()
            .find(|token| matches!(token.token, Token::Placeholder(_)))
            .unwrap();
        assert_eq!(bind.span.start, Location::new(1, 8));
        assert_eq!(bind.span.end, Location::new(1, 13));
    }

    #[test]
    fn full_token_pass_rejects_mixed_bind_styles() {
        assert_eq!(
            tokenize("SELECT :1, :name FROM T", &SqlLimits::default())
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn full_token_pass_rejects_positional_gaps_but_allows_repeats() {
        for sql in ["SELECT :2 FROM T", "SELECT :1, :3 FROM T"] {
            assert_eq!(
                tokenize(sql, &SqlLimits::default()).unwrap_err().code(),
                ErrorCode::Bind
            );
        }

        let tokens = tokenize("SELECT :1, :2, :1 FROM T", &SqlLimits::default()).unwrap();
        assert_eq!(placeholder_texts(&tokens), vec![":1", ":2", ":1"]);
    }

    #[test]
    fn invalid_adjacent_bind_forms_use_checked_parameter_parsing() {
        for sql in [
            "SELECT :0 FROM T",
            "SELECT :01 FROM T",
            "SELECT :65536 FROM T",
            "SELECT :1abc FROM T",
        ] {
            assert_eq!(
                tokenize(sql, &SqlLimits::default()).unwrap_err().code(),
                ErrorCode::Bind
            );
        }
    }

    #[test]
    fn owned_bind_grammar_accepts_underscore_and_combining_scalars_whole() {
        for bind in [":_name", ":i\u{307}x"] {
            let sql = format!("SELECT {bind} FROM T");
            let tokens = tokenize(&sql, &SqlLimits::default()).unwrap();
            assert_eq!(placeholder_texts(&tokens), vec![bind]);
        }
    }

    #[test]
    fn invalid_owned_bind_candidate_is_not_rewritten_as_a_valid_prefix() {
        assert_eq!(
            tokenize("SELECT :1\u{307}x FROM T", &SqlLimits::default())
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }
}
