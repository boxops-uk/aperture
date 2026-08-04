use super::parser::{Diagnostic, Span};
use codespan_reporting::diagnostic::Label;
use logos::Logos;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum LexerError {
    #[default]
    Invalid,
}

impl LexerError {
    pub fn into_diagnostic(self, span: Span) -> Diagnostic {
        match self {
            Self::Invalid => Diagnostic::error()
                .with_message("invalid token")
                .with_label(Label::primary((), span)),
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Logos, Debug, PartialEq, Copy, Clone)]
#[logos(error = LexerError)]
pub enum Token {
    EOF,
    #[regex(r"([ \t\n\f]|\\\n)+")]
    Whitespace,
    #[token("where")]
    Where,
    #[token("never")]
    Never,
    #[regex(r"[a-z][a-zA-Z0-9_]*(\.[a-z][a-zA-Z0-9_]*)*\.[A-Z][a-zA-Z0-9_]*")]
    QId,
    #[regex(r"[A-Z][a-zA-Z0-9_]*")]
    UId,
    #[regex(r"[a-z][a-zA-Z0-9_]*")]
    LId,
    #[token("_")]
    Wildcard,
    #[regex(r"[0-9][0-9_]*")]
    Nat,
    #[regex(r#""(?:\\(?:["\\/bfnrt]|u[a-fA-F0-9]{4})|[^"\\[:cntrl:]]+)*""#)]
    String,
    #[token("..")]
    DotDot,
    #[token(".")]
    Dot,
    #[token("=")]
    Eq,
    #[token(";")]
    Semi,
    #[token(",")]
    Comma,
    #[token("-")]
    Minus,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("(")]
    LPar,
    #[token(")")]
    RPar,
    Error,
}

pub fn tokenize(source: &str, diags: &mut Vec<Diagnostic>) -> (Vec<Token>, Vec<Span>) {
    let lexer = Token::lexer(source);
    let mut tokens = vec![];
    let mut spans = vec![];

    for (token, span) in lexer.spanned() {
        match token {
            Ok(token) => {
                tokens.push(token);
            }
            Err(err) => {
                diags.push(err.into_diagnostic(span.clone()));
                tokens.push(Token::Error);
            }
        }
        spans.push(span);
    }
    (tokens, spans)
}

/// Why a literal token's text does not denote a value.
///
/// The lexer deliberately accepts more than focus means. A malformed number is
/// **one `Nat` token**, not a token-boundary failure, so the reader gets "repeated
/// digit separator" pointing at the number rather than a parse error pointing
/// between two of them. Validation therefore happens here, on the token's text,
/// and lowering turns these into diagnostics with the token's span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralError {
    /// A digit separator that is not between digits: `1__0`, `1_`.
    IntSeparator,
    /// A leading zero on a multi-digit number: `007`.
    IntLeadingZero,
    /// Does not fit the target integer type.
    IntRange,
    /// A `\u` escape that is not a Unicode scalar value — an unpaired surrogate.
    StringEscape,
}

impl LiteralError {
    /// The diagnostic code, and so the identity the corpus asserts on.
    pub fn code(self) -> &'static str {
        match self {
            Self::IntSeparator => "lit/int-underscore",
            Self::IntLeadingZero => "lit/int-leading-zero",
            Self::IntRange => "lit/int-range",
            Self::StringEscape => "lit/string-escape",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::IntSeparator => "a digit separator must sit between digits",
            Self::IntLeadingZero => "a number must not have a leading zero",
            Self::IntRange => "number out of range",
            Self::StringEscape => "escape does not denote a character",
        }
    }
}

/// Decode a `Nat` token's text as a magnitude.
///
/// Enforces the shape the lexer's regex is deliberately looser than:
/// `0 | [1-9][0-9]*(_[0-9]+)*`.
///
/// Returns the magnitude, not a signed value: `-9223372036854775808` is a valid
/// `i64` whose magnitude is not, so the sign has to be applied before the range
/// is checked. That is [`signed_literal`].
pub fn parse_nat(text: &str) -> Result<u64, LiteralError> {
    // The lexer's `Nat` regex starts with a digit, so neither of these can come
    // from a real token; checked anyway rather than assumed.
    if text.is_empty() || text.starts_with('_') || text.ends_with('_') {
        return Err(LiteralError::IntSeparator);
    }
    if text.contains("__") {
        return Err(LiteralError::IntSeparator);
    }

    let digits: String = text.chars().filter(|c| *c != '_').collect();
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(LiteralError::IntLeadingZero);
    }

    digits.parse::<u64>().map_err(|_| LiteralError::IntRange)
}

/// Apply a sign to a magnitude, rejecting anything outside `i64`.
///
/// `i64::MIN` is only reachable through the unary minus — its magnitude is one
/// past `i64::MAX` — so this is where that asymmetry lives, and why a literal is
/// never parsed straight into an `i64`.
pub fn signed_literal(magnitude: u64, negative: bool) -> Result<i64, LiteralError> {
    if negative {
        if magnitude > 1 << 63 {
            return Err(LiteralError::IntRange);
        }
        // Negate in u64 then reinterpret: `-(1 << 63)` is i64::MIN, which cannot
        // be formed by negating any i64.
        Ok((magnitude.wrapping_neg()) as i64)
    } else {
        i64::try_from(magnitude).map_err(|_| LiteralError::IntRange)
    }
}

/// Decode a `String` token's text: quotes stripped, escapes resolved.
///
/// The lexer's regex already restricts *which* escapes can appear, so the only
/// failure left is a `\u` escape naming an unpaired surrogate. Surrogate pairs are
/// combined, as in JSON.
pub fn unescape_str(text: &str) -> Result<String, LiteralError> {
    // A `String` token always carries its quotes.
    let body = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .ok_or(LiteralError::StringEscape)?;

    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        match chars.next().ok_or(LiteralError::StringEscape)? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => out.push(unescape_code_point(&mut chars)?),
            _ => return Err(LiteralError::StringEscape),
        }
    }

    Ok(out)
}

/// One `\u`-escaped scalar value, joining a surrogate pair if it finds one.
fn unescape_code_point(chars: &mut std::str::Chars<'_>) -> Result<char, LiteralError> {
    let high = take_hex4(chars)?;

    let code_point = match high {
        // A high surrogate is only a character together with its low half.
        0xD800..=0xDBFF => {
            if chars.next() != Some('\\') || chars.next() != Some('u') {
                return Err(LiteralError::StringEscape);
            }
            let low = take_hex4(chars)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(LiteralError::StringEscape);
            }
            0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00)
        }
        // A low surrogate on its own never is.
        0xDC00..=0xDFFF => return Err(LiteralError::StringEscape),
        _ => high,
    };

    char::from_u32(code_point).ok_or(LiteralError::StringEscape)
}

fn take_hex4(chars: &mut std::str::Chars<'_>) -> Result<u32, LiteralError> {
    let mut value = 0u32;
    for _ in 0..4 {
        let digit = chars
            .next()
            .and_then(|c| c.to_digit(16))
            .ok_or(LiteralError::StringEscape)?;
        value = value * 16 + digit;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokens, minus the trivia the parser skips.
    fn tokens(source: &str) -> Vec<Token> {
        let (tokens, _) = tokenize(source, &mut vec![]);
        tokens
            .into_iter()
            .filter(|t| *t != Token::Whitespace)
            .collect()
    }

    /// A qualified predicate name needs an uppercase final segment. That is what
    /// keeps it apart from an access chain without any parser lookahead.
    #[test]
    fn qid_needs_an_uppercase_final_segment() {
        use Token::*;
        assert_eq!(tokens("test.Foo"), [QId]);
        assert_eq!(tokens("a.b.C"), [QId]);
        assert_eq!(tokens("test.foo"), [LId, Dot, LId]);
        // The qualified name ends at `.C`; `.c` is an access on it.
        assert_eq!(tokens("a.B.c"), [QId, Dot, LId]);
    }

    /// Field access on a variable is never a qualified name — variables are
    /// uppercase-leading and a `QId` is lowercase-leading.
    #[test]
    fn access_on_a_variable_is_not_a_qualified_name() {
        use Token::*;
        assert_eq!(tokens("E.from"), [UId, Dot, LId]);
        assert_eq!(tokens("X.name.inner"), [UId, Dot, LId, Dot, LId]);
    }

    /// `..` is one token by maximal munch, so a string prefix never lexes as two
    /// dots.
    #[test]
    fn dotdot_is_one_token() {
        use Token::*;
        assert_eq!(tokens("\"abc\".."), [String, DotDot]);
        assert_eq!(tokens("X.."), [UId, DotDot]);
        assert_eq!(tokens("X.a"), [UId, Dot, LId]);
        assert_eq!(tokens("X...a"), [UId, DotDot, Dot, LId]);
    }

    #[test]
    fn keywords_beat_identifiers() {
        use Token::*;
        assert_eq!(tokens("where"), [Where]);
        assert_eq!(tokens("wherever"), [LId]);
        assert_eq!(tokens("somewhere"), [LId]);
    }

    /// A malformed number is deliberately *one* token: that is what lets the
    /// diagnostic point at the number rather than between two tokens.
    #[test]
    fn malformed_numbers_still_lex_as_one_token() {
        use Token::*;
        assert_eq!(tokens("42"), [Nat]);
        assert_eq!(tokens("1_000"), [Nat]);
        assert_eq!(tokens("007"), [Nat]);
        assert_eq!(tokens("1__0"), [Nat]);
        assert_eq!(tokens("1_"), [Nat]);
        assert_eq!(tokens("-42"), [Minus, Nat]);
    }

    /// An unterminated or control-carrying string is a lex error, not a token.
    #[test]
    fn broken_strings_are_lex_errors() {
        let mut diags = vec![];
        let (got, _) = tokenize("\"abc", &mut diags);
        assert!(got.contains(&Token::Error));
        assert_eq!(diags.len(), 1);

        let mut diags = vec![];
        let (got, _) = tokenize("\"a\nb\"", &mut diags);
        assert!(
            got.contains(&Token::Error),
            "a raw newline is a control char"
        );
        assert!(!diags.is_empty());
    }

    #[test]
    fn nat_shape_is_validated_after_lexing() {
        assert_eq!(parse_nat("0"), Ok(0));
        assert_eq!(parse_nat("42"), Ok(42));
        assert_eq!(parse_nat("1_000"), Ok(1000));
        assert_eq!(parse_nat("1_000_000"), Ok(1_000_000));

        assert_eq!(parse_nat("1__0"), Err(LiteralError::IntSeparator));
        assert_eq!(parse_nat("1_"), Err(LiteralError::IntSeparator));
        assert_eq!(parse_nat("007"), Err(LiteralError::IntLeadingZero));
        assert_eq!(parse_nat("0_0"), Err(LiteralError::IntLeadingZero));
        assert_eq!(
            parse_nat("99999999999999999999"),
            Err(LiteralError::IntRange)
        );
    }

    /// The sign is applied before the range check, which is the only way to reach
    /// `i64::MIN` — and the reason a literal is never parsed straight into `i64`.
    #[test]
    fn i64_min_is_reachable_and_nothing_beyond_it_is() {
        assert_eq!(signed_literal(42, false), Ok(42));
        assert_eq!(signed_literal(42, true), Ok(-42));
        assert_eq!(signed_literal(0, true), Ok(0));

        let max = i64::MAX as u64;
        assert_eq!(signed_literal(max, false), Ok(i64::MAX));
        assert_eq!(signed_literal(max + 1, true), Ok(i64::MIN));

        // One past i64::MAX is only a number with a minus in front of it.
        assert_eq!(signed_literal(max + 1, false), Err(LiteralError::IntRange));
        assert_eq!(signed_literal(max + 2, true), Err(LiteralError::IntRange));
    }

    #[test]
    fn string_escapes_are_decoded() {
        assert_eq!(unescape_str("\"abc\"").as_deref(), Ok("abc"));
        assert_eq!(unescape_str("\"\"").as_deref(), Ok(""));
        assert_eq!(unescape_str(r#""a\nb""#).as_deref(), Ok("a\nb"));
        assert_eq!(
            unescape_str(r#""a\tb\\c\"d\/e""#).as_deref(),
            Ok("a\tb\\c\"d/e")
        );
        assert_eq!(unescape_str(r#""\b\f""#).as_deref(), Ok("\u{8}\u{c}"));
        assert_eq!(unescape_str(r#""A""#).as_deref(), Ok("A"));
        // Embedded NUL: the codec escapes it on the way to bytes, so the front end
        // must be able to carry one ([I1] key ordering depends on that escaping).
        assert_eq!(unescape_str(r#""\u0000""#).as_deref(), Ok("\0"));
        // A surrogate pair is one character.
        assert_eq!(unescape_str(r#""\uD83D\uDE00""#).as_deref(), Ok("😀"));
    }

    #[test]
    fn unpaired_surrogates_are_rejected() {
        for text in [
            r#""\uD800""#,       // high surrogate, nothing after it
            r#""\uDC00""#,       // low surrogate on its own
            r#""\uD800A""#,      // high surrogate followed by a non-surrogate
            r#""\uD800\uD800""#, // two high surrogates
            r#""\u00""#,         // truncated
            r#""\uZZZZ""#,       // not hex
        ] {
            assert_eq!(
                unescape_str(text),
                Err(LiteralError::StringEscape),
                "{text} must not decode"
            );
        }
    }
}
