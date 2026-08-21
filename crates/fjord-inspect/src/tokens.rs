//! The lexer's answer, as data.
//!
//! What the site had before this was a hand-written sigla highlighter in
//! JavaScript — a second implementation of the lexer, and so a second thing to
//! keep true. This is the first one: `where` is a keyword here because
//! [`fjord_engine::lexer`] says it is, and a token added to the language shows
//! up in the browser without anyone editing a regex.
//!
//! **The trap this module exists to avoid** is a view that loses bytes. A
//! highlighter that drops a character silently mis-aligns everything after it,
//! and the misalignment looks like a styling bug rather than a lexing one — so
//! `token_spans_reproduce_the_source_exactly` reassembles the source from the
//! view and compares.

use fjord_engine::lexer::{Token, tokenize};
use fjord_schema::syntax::lexer::{Token as SchemaToken, tokenize as tokenize_schema};
use serde::Serialize;

use crate::view::{DiagnosticView, Span, view_of};

/// What a token *is* to the language — not what it should look like.
///
/// A category rather than a colour: the palette is the page's decision (and the
/// terminal's is `fjord_cli::prompt::colour`'s), while which category a token
/// falls in is the language's. Splitting them is what stops a style choice from
/// being spelled as a lexer fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenClass {
    /// `where`, `never` — a word the grammar reserves.
    Keyword,
    /// A qualified predicate name: `src.File`.
    Predicate,
    /// A namespace, in a schema's `import`: `src.detail`.
    Namespace,
    /// A query variable: `X`, `Name`.
    Variable,
    /// A field or alternative name: `id`, `name`.
    Field,
    /// A number.
    Number,
    /// A string literal.
    String,
    /// `_`.
    Wildcard,
    /// Everything that separates or relates: braces, `=`, `!`, `|`, `..`.
    Punctuation,
    /// A `#` comment. Only the schema language has them — sigla does not, which
    /// is a fact about the language rather than an omission here.
    Comment,
    /// Spaces, tabs, newlines — carried, never dropped.
    Whitespace,
    /// Bytes the lexer could not read. A diagnostic points at the same span.
    Error,
}

/// One token: what it is, where it is, and the bytes it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenView {
    /// The variant's name, as the engine spells it (`QId`, `BangEq`).
    pub kind: &'static str,
    pub class: TokenClass,
    pub span: Span,
    pub text: String,
}

/// The whole of what lexing a source says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub tokens: Vec<TokenView>,
    pub diagnostics: Vec<DiagnosticView>,
}

/// Lex `source` and describe what came back.
///
/// Never fails: the lexer reports an unreadable byte as `Token::Error` plus a
/// diagnostic and carries on, which is what keeps the token stream aligned with
/// the source it came from ("permissive early, narrow later").
#[must_use]
pub fn tokens(source: &str) -> Tokens {
    let mut diagnostics = Vec::new();
    let (tokens, spans) = tokenize(source, &mut diagnostics);

    Tokens {
        tokens: tokens
            .iter()
            .zip(spans.iter())
            .map(|(token, span)| TokenView {
                kind: kind(*token),
                class: class(*token),
                span: Span {
                    start: span.start,
                    end: span.end,
                },
                text: source[span.clone()].to_owned(),
            })
            .collect(),
        diagnostics: diagnostics.iter().map(view_of).collect(),
    }
}

/// Lex `source` as a **schema** and describe what came back.
///
/// The schema language is a second language with a second lexer, and this is
/// that lexer — not a second reading of the first. The two agree where they
/// overlap and differ where the languages do: a schema has comments and
/// namespaces, sigla has neither.
#[must_use]
pub fn schema_tokens(source: &str) -> Tokens {
    let mut diagnostics = Vec::new();
    let (tokens, spans) = tokenize_schema(source, &mut diagnostics);

    Tokens {
        tokens: tokens
            .iter()
            .zip(spans.iter())
            .map(|(token, span)| TokenView {
                kind: schema_kind(*token),
                class: schema_class(*token),
                span: Span {
                    start: span.start,
                    end: span.end,
                },
                text: source[span.clone()].to_owned(),
            })
            .collect(),
        diagnostics: diagnostics.iter().map(view_of).collect(),
    }
}

/// The schema token view, already JSON.
#[must_use]
pub fn schema_tokens_json(source: &str) -> String {
    serde_json::to_string(&schema_tokens(source)).expect("a token view serialises")
}

/// The same view, already JSON.
///
/// Here rather than in the WebAssembly shell so that the string a browser
/// receives is the string the host suite asserts on: if serialising lived on
/// the other side of the boundary, "the same JSON on the host and in wasm"
/// would be a claim needing a test rather than a consequence of there being one
/// encoder.
#[must_use]
pub fn tokens_json(source: &str) -> String {
    // Infallible: the view is `derive(Serialize)` structs of strings, numbers
    // and enums, with no map keyed by anything but a string. A `Result` here
    // would make every caller handle an impossibility.
    serde_json::to_string(&tokens(source)).expect("a token view serialises")
}

/// The token's name, as a string a page can key on.
///
/// Written out rather than derived from `Debug`: a `{:?}` would make the
/// formatter's output a JSON contract, and this match is exhaustive on purpose —
/// a token added to sigla does not compile until somebody says what it is
/// called here.
pub(crate) const fn kind(token: Token) -> &'static str {
    match token {
        Token::EOF => "EOF",
        Token::Whitespace => "Whitespace",
        Token::Where => "Where",
        Token::Never => "Never",
        Token::QId => "QId",
        Token::UId => "UId",
        Token::LId => "LId",
        Token::Wildcard => "Wildcard",
        Token::Nat => "Nat",
        Token::String => "String",
        Token::DotDot => "DotDot",
        Token::Dot => "Dot",
        Token::Eq => "Eq",
        Token::BangEq => "BangEq",
        Token::Lt => "Lt",
        Token::Le => "Le",
        Token::Gt => "Gt",
        Token::Ge => "Ge",
        Token::Plus => "Plus",
        Token::Semi => "Semi",
        Token::Comma => "Comma",
        Token::Minus => "Minus",
        Token::LBrace => "LBrace",
        Token::RBrace => "RBrace",
        Token::LPar => "LPar",
        Token::RPar => "RPar",
        Token::Pipe => "Pipe",
        Token::Question => "Question",
        Token::Bang => "Bang",
        Token::Error => "Error",
    }
}

/// Which category a token falls in. Exhaustive for the same reason [`kind`] is.
const fn class(token: Token) -> TokenClass {
    match token {
        Token::Where | Token::Never => TokenClass::Keyword,
        Token::QId => TokenClass::Predicate,
        // A leading capital is a variable, and a qualified name is `QId` — the
        // lexer has already made that distinction, so a page never has to.
        Token::UId => TokenClass::Variable,
        Token::LId => TokenClass::Field,
        Token::Nat => TokenClass::Number,
        Token::String => TokenClass::String,
        Token::Wildcard => TokenClass::Wildcard,
        Token::Whitespace => TokenClass::Whitespace,
        Token::Error | Token::EOF => TokenClass::Error,
        Token::DotDot
        | Token::Dot
        | Token::Eq
        | Token::BangEq
        | Token::Lt
        | Token::Le
        | Token::Gt
        | Token::Ge
        | Token::Plus
        | Token::Semi
        | Token::Comma
        | Token::Minus
        | Token::LBrace
        | Token::RBrace
        | Token::LPar
        | Token::RPar
        | Token::Pipe
        | Token::Question
        | Token::Bang => TokenClass::Punctuation,
    }
}

/// The schema token's name. Exhaustive for the reason [`kind`] is: the schema
/// language has its own lexer, and a token added to it does not compile until
/// somebody says what it is called here.
const fn schema_kind(token: SchemaToken) -> &'static str {
    match token {
        SchemaToken::EOF => "EOF",
        SchemaToken::Whitespace => "Whitespace",
        SchemaToken::Comment => "Comment",
        SchemaToken::Schema => "Schema",
        SchemaToken::Import => "Import",
        SchemaToken::Predicate => "Predicate",
        SchemaToken::Type => "Type",
        SchemaToken::Derive => "Derive",
        SchemaToken::Stored => "Stored",
        SchemaToken::Evolves => "Evolves",
        SchemaToken::Enum => "Enum",
        SchemaToken::Maybe => "Maybe",
        SchemaToken::Set => "Set",
        SchemaToken::QId => "QId",
        SchemaToken::NsId => "NsId",
        SchemaToken::UId => "UId",
        SchemaToken::LId => "LId",
        SchemaToken::Nat => "Nat",
        SchemaToken::Arrow => "Arrow",
        SchemaToken::Colon => "Colon",
        SchemaToken::Comma => "Comma",
        SchemaToken::Eq => "Eq",
        SchemaToken::Pipe => "Pipe",
        SchemaToken::LBrace => "LBrace",
        SchemaToken::RBrace => "RBrace",
        SchemaToken::LBrack => "LBrack",
        SchemaToken::RBrack => "RBrack",
        SchemaToken::LPar => "LPar",
        SchemaToken::RPar => "RPar",
        SchemaToken::Error => "Error",
    }
}

/// Which category a schema token falls in.
///
/// The **same** categories the query language uses, so one stylesheet paints
/// both and a reader meets one vocabulary rather than two. Where the two
/// languages spell the same idea differently the class is what agrees: a
/// predicate name is `Predicate` in both, whether it is being declared or
/// queried.
const fn schema_class(token: SchemaToken) -> TokenClass {
    match token {
        SchemaToken::Schema
        | SchemaToken::Import
        | SchemaToken::Predicate
        | SchemaToken::Type
        | SchemaToken::Derive
        | SchemaToken::Stored
        | SchemaToken::Evolves
        | SchemaToken::Enum
        | SchemaToken::Maybe
        | SchemaToken::Set => TokenClass::Keyword,
        SchemaToken::Comment => TokenClass::Comment,
        SchemaToken::QId => TokenClass::Predicate,
        SchemaToken::NsId => TokenClass::Namespace,
        // A schema declares types where a query names variables, and both are
        // the capitalised word: `string` in `name : string` is the type, and a
        // bare `File` is the predicate being declared.
        SchemaToken::UId => TokenClass::Variable,
        SchemaToken::LId => TokenClass::Field,
        SchemaToken::Nat => TokenClass::Number,
        SchemaToken::Whitespace => TokenClass::Whitespace,
        SchemaToken::Error | SchemaToken::EOF => TokenClass::Error,
        SchemaToken::Arrow
        | SchemaToken::Colon
        | SchemaToken::Comma
        | SchemaToken::Eq
        | SchemaToken::Pipe
        | SchemaToken::LBrace
        | SchemaToken::RBrace
        | SchemaToken::LBrack
        | SchemaToken::RBrack
        | SchemaToken::LPar
        | SchemaToken::RPar => TokenClass::Punctuation,
    }
}
