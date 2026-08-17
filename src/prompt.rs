//! **The prompt itself** — what both shells share, and nothing either of them means.
//!
//! There are two shells and there will go on being two
//! ([operations §5](../docs/aperture-cli-design.md)): [`crate::commands::shell`] is the
//! product one, always over the wire, and [`crate::shell`] is Phase 5's embedded demo
//! over a scratch database it seeds itself. What they have in common is a *terminal* —
//! the same highlighter over the same lexer, the same rule for where a line's focus
//! source begins, the same decision about colour, and the same table shape behind
//! `:help`. That is this module. What differs is every command's meaning, which is why
//! the table is passed in rather than defined here.
//!
//! # Colour is decided once, and off a pipe it is off
//!
//! [`colours_enabled`] answers `NO_COLOR` and "is stdout a terminal" once per process.
//! Everything that paints asks it, so a piped shell writes plain text and a test
//! capturing into a `Vec<u8>` sees exactly what it asserts on.

use std::{
    borrow::Cow,
    io::IsTerminal,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use aperture_engine::{
    lexer::{Token, tokenize},
    syntax::Ty,
};
use aperture_schema::schema::{LocalInterner, Schema};
use rustyline::{
    Context, Helper,
    completion::{Completer, Pair},
    highlight::{CmdKind, Highlighter},
    hint::Hinter,
    validate::{ValidationContext, ValidationResult, Validator},
};

/// One shell command, as `:help`, the highlighter and the completer all read it.
///
/// `argument` is `Some` when the command takes **focus source** — a query, or a
/// predicate name. That is the field the highlighter reads, and it is why this is a
/// table rather than three lists: a command added to the table without an arm in the
/// dispatch is advertised, highlighted and inert, which is the one drift worth
/// designing against.
pub struct Command {
    pub name: &'static str,
    /// Other spellings that reach the same arm — the `\d` a hand trained on psql
    /// types, and the one-letter forms.
    ///
    /// They are **not** advertised by `:help`: a help screen listing every spelling
    /// twice is a help screen nobody reads to the end.
    pub aliases: &'static [&'static str],
    pub argument: Option<&'static str>,
    pub help: &'static str,
}

impl Command {
    /// Whether `word` names this command, by its name or by any alias.
    #[must_use]
    pub fn answers_to(&self, word: &str) -> bool {
        self.name == word || self.aliases.contains(&word)
    }
}

/// Whether anything should be painted at all.
///
/// `NO_COLOR` first, then "is this a terminal" — the two conventions every tool that
/// gets this right observes, and between them they cover a pipe, a CI log and a person
/// who has said no.
#[must_use]
pub fn colours_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// Wrap `text` in an ANSI code, or hand it back untouched.
#[must_use]
pub fn painted(code: &str, text: &str) -> String {
    if colours_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// ANSI colours, chosen by what a token *means* rather than what it is: someone
/// scanning a query wants predicates, variables and literals to separate.
#[must_use]
pub fn colour(token: Token) -> &'static str {
    match token {
        // A lexer error, marked as it is typed — the earliest diagnostic there is.
        Token::Error => "1;31",
        Token::Where | Token::Never => "1;35",
        Token::QId => "33",
        Token::UId => "1;36",
        Token::LId => "34",
        Token::Nat | Token::String | Token::Minus | Token::DotDot => "32",
        Token::Wildcard | Token::Pipe | Token::Bang | Token::Question => "1;90",
        _ => "90",
    }
}

/// The same palette, for the **schema** language.
///
/// Two lexers, one set of colours: a predicate name is the same yellow in a schema as
/// in a query, and a keyword the same magenta. Until Phase 8 the schema had no lexer
/// and `:schema` was painted by a hand-written tokeniser that guessed; there is a real
/// one now, so what is printed is schema source painted by the schema's own lexer.
#[must_use]
pub fn schema_colour(token: aperture_schema::syntax::lexer::Token) -> &'static str {
    use aperture_schema::syntax::lexer::Token as S;

    match token {
        S::Error => "1;31",
        S::Comment => "90",
        S::Schema | S::Import | S::Predicate | S::Type | S::Derive | S::Stored | S::Evolves => {
            "1;35"
        }
        // `int` and `string` lex as ordinary lowercase names, so they are named here
        // rather than distinguished by the lexer — the one place this palette knows
        // something the grammar does not.
        S::LId => "34",
        S::QId | S::NsId => "33",
        S::UId => "1;36",
        S::Nat => "32",
        _ => "90",
    }
}

/// Paint schema source with the schema lexer.
#[must_use]
pub fn paint_schema(source: &str) -> String {
    if !colours_enabled() {
        return source.to_owned();
    }

    let (tokens, spans) = aperture_schema::syntax::lexer::tokenize(source, &mut Vec::new());
    let mut out = String::with_capacity(source.len() * 2);
    let mut last = 0;

    for (token, span) in tokens.iter().zip(spans.iter()) {
        if span.start > last {
            out.push_str(&source[last..span.start]);
        }

        // A builtin type is an `LId` like a field name, and telling them apart is
        // most of why anyone wants this coloured. The lexer cannot: the grammar does
        // not reserve them.
        let code = match &source[span.clone()] {
            "int" | "string" => "1;35",
            _ => schema_colour(*token),
        };

        if matches!(
            token,
            aperture_schema::syntax::lexer::Token::Whitespace
                | aperture_schema::syntax::lexer::Token::EOF
        ) {
            out.push_str(&source[span.clone()]);
        } else {
            out.push_str("\x1b[");
            out.push_str(code);
            out.push('m');
            out.push_str(&source[span.clone()]);
            out.push_str("\x1b[0m");
        }

        last = span.end;
    }

    if last < source.len() {
        out.push_str(&source[last..]);
    }

    out
}

/// A head type, as the prompt shows it.
///
/// Shared by both shells because it is the same answer to the same question: `:type`
/// prints it alone, and a query prints it above its rows. Nothing here is coloured —
/// a type is short and reads as one thing, where a *schema* is a page and needs its
/// parts told apart.
#[must_use]
pub fn render_ty(ty: &Ty, schema: &Schema, interner: &LocalInterner) -> String {
    match ty {
        Ty::Int => "int".to_owned(),
        Ty::String => "str".to_owned(),
        Ty::Error => "?error".to_owned(),
        Ty::Var(_) => "?".to_owned(),
        Ty::Fact(predicate) => schema
            .get(*predicate)
            .and_then(|p| p.name())
            .map_or_else(|| "fact".to_owned(), str::to_owned),
        Ty::Record(fields) => {
            let rendered = fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{}: {}",
                        interner.try_resolve(*name).unwrap_or("?"),
                        render_ty(field, schema, interner)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{rendered}}}")
        }
    }
}

/// Where a **line's** history is kept between sessions.
///
/// `$XDG_STATE_HOME` rather than the data directory, because that is what the spec is
/// for — history is state a person accumulates, not data a database holds — and
/// putting it under the store root would mean a history file inside every backup.
#[must_use]
pub fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;

    Some(base.join("aperture").join("history"))
}

/// The line editor's helper: highlighting, hints, completion and when a line is done.
pub struct FocusHelper {
    commands: &'static [Command],
    /// Predicate names to complete, refreshed when the session's schema changes.
    ///
    /// Behind a lock because rustyline owns the helper and the shell owns the
    /// connection, and `:connect` changes what the answers are. A `Mutex` rather than
    /// a channel for the same reason a shell is not a server: there is one writer, it
    /// writes between lines, and the reader is a keystroke.
    names: Mutex<Vec<String>>,
}

impl FocusHelper {
    #[must_use]
    pub fn new(commands: &'static [Command]) -> FocusHelper {
        FocusHelper {
            commands,
            names: Mutex::new(vec![]),
        }
    }

    /// Replace the predicate names offered on tab.
    pub fn knows(&self, names: Vec<String>) {
        if let Ok(mut held) = self.names.lock() {
            *held = names;
        }
    }

    /// Where this line's **focus source** begins, if any.
    ///
    /// A query is source from the first byte. A command word is not — but the
    /// *argument* of a command that takes one is, so `:plan X where …` and
    /// `:facts src.Decl` colour everything past the word. `None` means there is no
    /// source in the line at all: a bare command, a command that takes no argument, or
    /// one the shell does not know.
    ///
    /// Keyed on the command table rather than on "everything after the first word",
    /// which is the point rather than an implementation detail: colour appearing as you
    /// type the argument is also the shell saying it **recognised the command**. A typo
    /// stays grey.
    fn source_offset(&self, line: &str) -> Option<usize> {
        let trimmed = line.trim_start();

        if !starts_a_command(trimmed) {
            return Some(0);
        }

        // No whitespace yet ⇒ the command word is still being typed, and there is no
        // argument to colour.
        let word_end = trimmed.find(char::is_whitespace)?;
        let indent = line.len() - trimmed.len();
        let word = &trimmed[..word_end];

        self.commands
            .iter()
            .any(|command| command.answers_to(word) && command.argument.is_some())
            .then_some(indent + word_end)
    }

    /// Paint `source` onto `out`, one colour per token.
    ///
    /// Pushes only slices of `source` and fixed colour codes, which is what keeps
    /// highlighting byte-preserving — it runs on every keystroke over half-typed input,
    /// so losing a byte would show the wrong text under the cursor.
    fn paint(source: &str, out: &mut String) {
        // Diagnostics discarded: they belong to submitting a line, not to typing one.
        // What is live here is the colour — an invalid token turns red under the
        // cursor.
        let (tokens, spans) = tokenize(source, &mut Vec::new());
        let mut last = 0;

        for (token, span) in tokens.iter().zip(spans.iter()) {
            if span.start > last {
                out.push_str(&source[last..span.start]);
            }

            // Whitespace carries no colour: painting it would colour the gaps between
            // tokens as well as the tokens.
            if matches!(token, Token::Whitespace) {
                out.push_str(&source[span.clone()]);
            } else {
                out.push_str("\x1b[");
                out.push_str(colour(*token));
                out.push('m');
                out.push_str(&source[span.clone()]);
                out.push_str("\x1b[0m");
            }

            last = span.end;
        }

        if last < source.len() {
            out.push_str(&source[last..]);
        }
    }
}

/// Whether a line is a command rather than a query.
///
/// **Both prefixes**, and that is a decision rather than indulgence: `:` is what the
/// demo has always used and what this tool's own commands are named after, and `\` is
/// what a hand trained on psql types without thinking. Neither can be the start of a
/// focus query, so accepting both costs nothing and refusing one costs somebody a
/// puzzled minute.
#[must_use]
pub fn starts_a_command(line: &str) -> bool {
    line.starts_with(':') || line.starts_with('\\')
}

/// Split a command line into its word and its argument, prefix removed.
#[must_use]
pub fn split_command(line: &str) -> (&str, &str) {
    let line = line.trim();

    match line.split_once(char::is_whitespace) {
        Some((word, argument)) => (word, argument.trim()),
        None => (line, ""),
    }
}

impl Highlighter for FocusHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let Some(at) = self.source_offset(line) else {
            return Cow::Borrowed(line);
        };

        let (command, source) = line.split_at(at);
        if source.trim().is_empty() {
            return Cow::Borrowed(line);
        }

        let mut out = String::with_capacity(line.len() * 2);
        out.push_str(command);
        Self::paint(source, &mut out);
        Cow::Owned(out)
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        true
    }
}

impl Hinter for FocusHelper {
    type Hint = String;

    /// A live hint for the one fault that is unambiguous mid-typing.
    ///
    /// Lexical only. Half-written input is a *parse* error almost continuously — `X
    /// where` is incomplete rather than wrong — so hinting those would be noise. An
    /// invalid token stays wrong however much more is typed.
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() {
            return None;
        }

        let source = &line[self.source_offset(line)?..];

        let (tokens, _) = tokenize(source, &mut Vec::new());
        tokens
            .iter()
            .any(|token| matches!(token, Token::Error))
            .then(|| "   (not a token)".to_owned())
    }
}

impl Completer for FocusHelper {
    type Candidate = Pair;

    /// Complete a command word, or a predicate name.
    ///
    /// The two are the only things in a line that come from a **closed** set. A
    /// variable is whatever the person just invented, a literal is theirs, and
    /// completing either would be guessing at what they meant; a predicate name is in
    /// the schema the server sent, and a command is in the table above.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let head = &line[..pos];

        // The word under the cursor, which for a name may carry dots.
        let start = head
            .rfind(|c: char| c.is_whitespace() || "{}(),;".contains(c))
            .map_or(0, |at| at + 1);
        let word = &head[start..];

        if word.is_empty() {
            return Ok((start, vec![]));
        }

        let mut matches: Vec<Pair> = vec![];

        if start == 0 && starts_a_command(word) {
            for command in self.commands {
                if command.name.starts_with(word) {
                    matches.push(Pair {
                        display: command.name.to_owned(),
                        replacement: format!("{} ", command.name),
                    });
                }
            }

            return Ok((start, matches));
        }

        if let Ok(names) = self.names.lock() {
            for name in names.iter() {
                if name.starts_with(word) {
                    matches.push(Pair {
                        display: name.clone(),
                        replacement: name.clone(),
                    });
                }
            }
        }

        Ok((start, matches))
    }
}

/// Whether a line wants another one after it.
///
/// **Unclosed brackets, and nothing cleverer.** A focus query has no terminator — `;`
/// separates statements *inside* one — so there is no punctuation that means "done",
/// and guessing from the grammar would make every half-typed `X where` a multi-line
/// prompt. What is unambiguous is an open `{` or `(`: nothing that closes them can be
/// on this line, so the line is not finished. Ctrl-C abandons it, as everywhere.
#[must_use]
pub fn is_incomplete(line: &str) -> bool {
    // A command is always one line: `:limit 40` cannot be incomplete, and a stray
    // brace in one should not swallow the next thing typed.
    if starts_a_command(line.trim_start()) {
        return false;
    }

    let (tokens, _) = tokenize(line, &mut Vec::new());
    let mut depth = 0i32;

    for token in &tokens {
        match token {
            Token::LBrace | Token::LPar => depth += 1,
            Token::RBrace | Token::RPar => depth -= 1,
            // A line that does not lex is wrong rather than unfinished, and the hinter
            // has already said so under the cursor.
            Token::Error => return false,
            _ => {}
        }
    }

    depth > 0
}

impl Validator for FocusHelper {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(if is_incomplete(ctx.input()) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl Helper for FocusHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: [Command; 2] = [
        Command {
            name: ":plan",
            aliases: &[],
            argument: Some("<query>"),
            help: "the plan it compiles to",
        },
        Command {
            name: ":quit",
            aliases: &["\\q"],
            argument: None,
            help: "leave",
        },
    ];

    fn helper() -> FocusHelper {
        FocusHelper::new(&COMMANDS)
    }

    /// A query is source from the first byte; a command's argument is source only when
    /// the command takes one, which is what makes colour a signal that the word was
    /// recognised.
    #[test]
    fn source_begins_where_the_command_ends() {
        let helper = helper();

        assert_eq!(helper.source_offset("X where src.File X"), Some(0));
        assert_eq!(helper.source_offset(":plan X where …"), Some(5));
        assert_eq!(helper.source_offset(":quit now"), None, "it takes none");
        assert_eq!(helper.source_offset(":pla"), None, "still being typed");
        assert_eq!(helper.source_offset(":nope X"), None, "not a command");
    }

    /// An alias reaches the same command, and `\` is as good a prefix as `:`.
    #[test]
    fn both_prefixes_start_a_command() {
        assert!(starts_a_command(":quit"));
        assert!(starts_a_command("\\q"));
        assert!(!starts_a_command("X where src.File X"));

        assert!(COMMANDS[1].answers_to("\\q"));
        assert!(COMMANDS[1].answers_to(":quit"));
        assert!(!COMMANDS[1].answers_to(":q"));
    }

    /// A line with an open brace wants another; anything else is finished. The
    /// **error** case is the one worth pinning: a line that does not lex is wrong
    /// rather than unfinished, and continuing it would trap somebody inside a typo.
    #[test]
    fn an_unclosed_bracket_is_what_continues_a_line() {
        assert!(is_incomplete("{file = F,"));
        assert!(is_incomplete("X where (A"));

        assert!(!is_incomplete("X where src.File X"));
        assert!(!is_incomplete("{file = F} where src.File F"));
        assert!(!is_incomplete(":limit 40"));
        assert!(
            !is_incomplete("{ £"),
            "a line that does not lex is not unfinished"
        );
        assert!(!is_incomplete("}"), "and a stray close is wrong, not open");
    }

    #[test]
    fn a_command_line_splits_into_word_and_argument() {
        assert_eq!(split_command(":plan X where Y"), (":plan", "X where Y"));
        assert_eq!(split_command("  :quit  "), (":quit", ""));
        assert_eq!(split_command(":limit 40"), (":limit", "40"));
    }
}
