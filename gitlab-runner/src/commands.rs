//! Helpers for parsing custom job commands with clap
//!
//! Enable the `commands` feature and implement [`CommandHandler`] with a
//! `clap::Parser` type, then wrap it in a [`CommandHandlerAdapter`].

use clap::{Parser, error::ErrorKind};
use std::{borrow::Cow, collections::HashSet};

use crate::{JobHandler, JobResult, NoFiles, Phase, UploadableFile, job::Job};

const MAX_VARIABLE_EXPANSIONS: usize = 1024;
const MAX_VARIABLE_EXPANSION_DEPTH: usize = 64;

/// An error encountered while parsing a job command
#[derive(Debug, thiserror::Error)]
pub enum ParseCommandError {
    /// The entry contained an unterminated quote
    #[error("unterminated quote")]
    InvalidQuoting,
    /// Variable expansion exceeded its reference count or nesting limit
    #[error("variable expansion limit exceeded")]
    ExpansionLimit,
    /// Clap rejected the command or its arguments
    #[error("{0}")]
    Arguments(#[source] clap::Error),
    /// Clap-generated help text for the handler to display
    #[error("{0}")]
    DisplayHelp(#[source] clap::Error),
    /// Clap-generated version text for the handler to display
    #[error("{0}")]
    DisplayVersion(#[source] clap::Error),
}

/// Handles typed commands from each job phase
///
/// Wrap an implementation in [`CommandHandlerAdapter`] to use it as a
/// [`JobHandler`]. Parsing errors, including help and version requests, are
/// passed to [`parse_error`](Self::parse_error) for the handler to report and
/// decide whether execution should continue.
#[async_trait::async_trait]
pub trait CommandHandler<U = NoFiles>: Send
where
    U: UploadableFile + Send + 'static,
{
    /// The clap parser defining the commands accepted by this handler
    type Command: Parser + Send;

    /// The job whose variables are used when parsing commands
    fn job(&self) -> &Job;

    /// Called before each entry is parsed, including blank entries
    ///
    /// The entry is unexpanded and may itself contain sensitive information.
    fn on_entry(&mut self, _entry: &str, _phase: Phase) {}

    /// Execute a parsed command. Returning an error stops the current phase.
    async fn command(&mut self, command: Self::Command, phase: Phase) -> JobResult;

    /// Handle a parse error. Returning an error stops the current phase.
    ///
    /// The error can contain expanded variables, including secrets.
    async fn parse_error(
        &mut self,
        entry: &str,
        phase: Phase,
        error: ParseCommandError,
    ) -> JobResult;

    /// Get the files available to upload, as in [`JobHandler::get_uploadable_files`]
    async fn get_uploadable_files(&mut self) -> Result<Box<dyn Iterator<Item = U> + Send>, ()> {
        Ok(Box::new(core::iter::empty()))
    }

    /// Clean up after the job, as in [`JobHandler::cleanup`]
    async fn cleanup(&mut self) {}
}

/// Adapts a [`CommandHandler`] to the runner's [`JobHandler`] interface
///
/// Job variables are recursively expanded before tokenization and their values
/// are shell-quoted, preserving spaces in unquoted variable references. Missing
/// variables and recursive references expand to an empty string, including in
/// `${NAME:-default}` expressions; `$$` escapes a dollar sign. Expansion does
/// not inspect surrounding quotes and is not equivalent to shell expansion.
/// Each entry permits at most 1024 variable references and 64 nested variables;
/// exceeding either limit is reported as [`ParseCommandError::ExpansionLimit`]
///
/// Each entry is tokenized with shell-style quoting and escaping, but is not
/// executed by a shell: pipelines and multiple shell commands are not supported.
/// The first token is a command argument, not a program name. Blank and
/// comment-only entries are skipped; invalid quoting and arguments, as well as
/// help and version requests, are passed to [`CommandHandler::parse_error`]
///
/// The adapter inherits [`JobHandler`]'s automatic cancellation behavior: an
/// active step is dropped on cancellation, and cleanup is still called
pub struct CommandHandlerAdapter<H>(H);

impl<H> CommandHandlerAdapter<H> {
    /// Wrap a command handler for use with a runner
    pub fn new(handler: H) -> Self {
        Self(handler)
    }
}

#[async_trait::async_trait]
impl<H, U> JobHandler<U> for CommandHandlerAdapter<H>
where
    H: CommandHandler<U>,
    U: UploadableFile + Send + 'static,
{
    async fn step(&mut self, script: &[String], phase: Phase) -> JobResult {
        for entry in script {
            self.0.on_entry(entry, phase);
            match parse_with_lookup::<H::Command>(entry, |name| {
                self.0.job().variable(name).map(|value| value.value())
            }) {
                Ok(Some(command)) => self.0.command(command, phase).await?,
                Ok(None) => {}
                Err(error) => self.0.parse_error(entry, phase, error).await?,
            }
        }
        Ok(())
    }

    async fn get_uploadable_files(&mut self) -> Result<Box<dyn Iterator<Item = U> + Send>, ()> {
        self.0.get_uploadable_files().await
    }

    async fn cleanup(&mut self) {
        self.0.cleanup().await;
    }
}

fn expand_vars<'s, 'v, F>(
    input: &'s str,
    quote: bool,
    expanding: &mut HashSet<String>,
    remaining: &mut usize,
    lookup: &F,
) -> Result<Cow<'s, str>, ParseCommandError>
where
    F: Fn(&str) -> Option<&'v str>,
{
    let mut error = None;
    let expanded = shellexpand::env_with_context_no_errors(input, |name| {
        if error.is_some() {
            return Some(String::new());
        }

        match expand_variable(name, quote, expanding, remaining, lookup) {
            Ok(value) => Some(value),
            Err(cause) => {
                // Keep errors out of shellexpand, which suppresses them for defaults.
                error = Some(cause);
                Some(String::new())
            }
        }
    });
    match error {
        Some(error) => Err(error),
        None => Ok(expanded),
    }
}

fn expand_variable<'v, F>(
    name: &str,
    quote: bool,
    expanding: &mut HashSet<String>,
    remaining: &mut usize,
    lookup: &F,
) -> Result<String, ParseCommandError>
where
    F: Fn(&str) -> Option<&'v str>,
{
    *remaining = remaining
        .checked_sub(1)
        .ok_or(ParseCommandError::ExpansionLimit)?;
    if expanding.contains(name) {
        return Ok(String::new());
    }
    if expanding.len() >= MAX_VARIABLE_EXPANSION_DEPTH {
        return Err(ParseCommandError::ExpansionLimit);
    }
    expanding.insert(name.to_owned());

    let value = lookup(name).unwrap_or("");
    let expanded = expand_vars(value, false, expanding, remaining, lookup);
    expanding.remove(name);
    let expanded = expanded?;
    Ok(if quote {
        shell_words::quote(&expanded).into_owned()
    } else {
        expanded.into_owned()
    })
}

fn parse_with_lookup<'v, C: Parser>(
    entry: &str,
    lookup: impl Fn(&str) -> Option<&'v str>,
) -> Result<Option<C>, ParseCommandError> {
    let mut remaining = MAX_VARIABLE_EXPANSIONS;
    let expanded = expand_vars(entry, true, &mut HashSet::new(), &mut remaining, &lookup)?;
    let tokens = shell_words::split(&expanded).map_err(|_| ParseCommandError::InvalidQuoting)?;
    if tokens.is_empty() {
        return Ok(None);
    }

    let result = C::command()
        .no_binary_name(true)
        .try_get_matches_from(tokens)
        .and_then(|matches| C::from_arg_matches(&matches));
    match result {
        Ok(command) => Ok(Some(command)),
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp => Err(ParseCommandError::DisplayHelp(error)),
            ErrorKind::DisplayVersion => Err(ParseCommandError::DisplayVersion(error)),
            _ => Err(ParseCommandError::Arguments(error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Subcommand;

    #[derive(Debug, Parser, PartialEq)]
    #[command(version)]
    struct Command {
        #[command(subcommand)]
        action: Action,
    }

    #[derive(Debug, Subcommand, PartialEq)]
    enum Action {
        Greet { name: String, count: Option<u32> },
    }

    fn parse_command<C: Parser>(entry: &str) -> Result<Option<C>, ParseCommandError> {
        parse_with_lookup::<C>(entry, |_| None)
    }

    #[test]
    fn expands_variables_before_parsing() {
        let variables = |name: &str| match name {
            "NAME" => Some("Jane Doe"),
            "NESTED" => Some("$NAME"),
            "RECURSIVE" => Some("recursion($RECURSIVE)"),
            _ => None,
        };
        for (entry, expected) in [
            ("greet $NAME", "Jane Doe"),
            ("greet $NESTED", "Jane Doe"),
            ("greet $RECURSIVE", "recursion()"),
            ("greet $MISSING", ""),
            ("greet ${MISSING:-fallback}", ""),
            ("greet $$NAME", "$NAME"),
        ] {
            let command = parse_with_lookup::<Command>(entry, variables)
                .unwrap()
                .unwrap();
            assert_eq!(
                command,
                Command {
                    action: Action::Greet {
                        name: expected.into(),
                        count: None,
                    },
                }
            );
        }
    }

    #[test]
    fn preserves_context_dependent_cycles() {
        let command = parse_with_lookup::<Command>("greet $A/$B", |name| match name {
            "A" => Some("a$B"),
            "B" => Some("b$A"),
            _ => None,
        })
        .unwrap()
        .unwrap();

        assert_eq!(
            command,
            Command {
                action: Action::Greet {
                    name: "ab/ba".into(),
                    count: None,
                },
            }
        );
    }

    #[test]
    fn parses_quoted_and_escaped_arguments() {
        for (entry, name, count) in [
            (r#"greet "Jane Doe" 2"#, "Jane Doe", Some(2)),
            (r"greet Jane\ Doe", "Jane Doe", None),
            ("greet Jane\\", "Jane\\", None),
        ] {
            let Some(command) = parse_command::<Command>(entry).unwrap() else {
                panic!("expected command for {entry}");
            };
            assert_eq!(
                command,
                Command {
                    action: Action::Greet {
                        name: name.into(),
                        count,
                    },
                }
            );
        }
    }

    #[test]
    fn returns_empty_and_display_outcomes() {
        for entry in ["", "  ", "# comment"] {
            assert!(matches!(parse_command::<Command>(entry), Ok(None)));
        }
        for (entry, kind) in [
            ("--help", ErrorKind::DisplayHelp),
            ("--version", ErrorKind::DisplayVersion),
            ("greet --help", ErrorKind::DisplayHelp),
        ] {
            let output = match (parse_command::<Command>(entry), kind) {
                (Err(ParseCommandError::DisplayHelp(output)), ErrorKind::DisplayHelp)
                | (Err(ParseCommandError::DisplayVersion(output)), ErrorKind::DisplayVersion) => {
                    output
                }
                _ => panic!("expected display output for {entry}"),
            };
            assert_eq!(output.kind(), kind);
            assert!(!output.to_string().is_empty());
        }
    }

    #[test]
    fn rejects_invalid_commands() {
        for entry in ["greet 'unterminated", "greet \"unterminated"] {
            assert!(matches!(
                parse_command::<Command>(entry),
                Err(ParseCommandError::InvalidQuoting)
            ));
        }
        for entry in ["unknown", "greet", "greet Jane nope"] {
            assert!(matches!(
                parse_command::<Command>(entry),
                Err(ParseCommandError::Arguments(_))
            ));
        }
    }
}
