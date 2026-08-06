mod command;
mod job;

use std::ffi::OsString;
use std::io;
use std::iter;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Result, bail};
use argmax::Command;

use crate::exec::command::OutputBuffer;
use crate::exit_codes::{ExitCode, merge_exitcodes};
use crate::fmt::{FormatTemplate, Token};

use self::command::{execute_commands, handle_cmd_error};
pub use self::job::{batch, job};

/// Execution mode of the command
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Command is executed for each search result
    OneByOne,
    /// Command is run for a batch of results at once
    Batch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandSet {
    mode: ExecutionMode,
    commands: Vec<CommandTemplate>,
}

impl CommandSet {
    pub fn new<I, T, S>(input: I) -> Result<CommandSet>
    where
        I: IntoIterator<Item = T>,
        T: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Ok(CommandSet {
            mode: ExecutionMode::OneByOne,
            commands: input
                .into_iter()
                .map(|args| CommandTemplate::new(args, ExecutionMode::OneByOne))
                .collect::<Result<_>>()?,
        })
    }

    pub fn new_batch<I, T, S>(input: I) -> Result<CommandSet>
    where
        I: IntoIterator<Item = T>,
        T: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Ok(CommandSet {
            mode: ExecutionMode::Batch,
            commands: input
                .into_iter()
                .map(|args| {
                    let cmd = CommandTemplate::new(args, ExecutionMode::Batch)?;
                    if cmd.number_of_path_args() > 1 {
                        bail!("Only one placeholder allowed for batch commands");
                    }
                    Ok(cmd)
                })
                .collect::<Result<Vec<_>>>()?,
        })
    }

    pub fn in_batch_mode(&self) -> bool {
        self.mode == ExecutionMode::Batch
    }

    pub fn execute(
        &self,
        input: &Path,
        path_separator: Option<&str>,
        null_separator: bool,
        buffer_output: bool,
    ) -> ExitCode {
        let commands = self
            .commands
            .iter()
            .map(|c| c.generate(input, path_separator));
        execute_commands(commands, OutputBuffer::new(null_separator), buffer_output)
    }

    pub fn execute_batch<I>(&self, paths: I, limit: usize, path_separator: Option<&str>) -> ExitCode
    where
        I: Iterator<Item = PathBuf>,
    {
        let mut jobs = JobCounter::default();
        let builders: io::Result<Vec<_>> = self
            .commands
            .iter()
            .map(|c| CommandBuilder::new(c, limit, &mut jobs))
            .collect();

        match builders {
            Ok(mut builders) => {
                for path in paths {
                    for builder in &mut builders {
                        if let Err(e) = builder.push(&path, path_separator, &mut jobs) {
                            return handle_cmd_error(Some(&builder.cmd), e);
                        }
                    }
                }

                for builder in &mut builders {
                    if let Err(e) = builder.finish(&mut jobs) {
                        return handle_cmd_error(Some(&builder.cmd), e);
                    }
                }

                merge_exitcodes(builders.iter().map(|b| b.exit_code()))
            }
            Err(e) => handle_cmd_error(None, e),
        }
    }
}

/// Hands out the numbers substituted for the `{#}` placeholder.
///
/// A number is reserved for every process a `--exec-batch` run is about to build, so it is
/// unique across all of that run's processes — including those of separate `-X` templates.
#[derive(Debug, Default)]
struct JobCounter(usize);

impl JobCounter {
    fn next_job(&mut self) -> usize {
        self.0 += 1;
        self.0
    }
}

/// A batch command's arguments, split around the path placeholder and bound to one job.
#[derive(Debug)]
struct JobArgs {
    pre: Vec<OsString>,
    path: FormatTemplate,
    post: Vec<OsString>,
}

/// Represents a multi-exec command as it is built.
#[derive(Debug)]
struct CommandBuilder<'a> {
    template: &'a CommandTemplate,
    args: JobArgs,
    cmd: Command,
    count: usize,
    limit: usize,
    exit_code: ExitCode,
}

impl<'a> CommandBuilder<'a> {
    fn new(template: &'a CommandTemplate, limit: usize, jobs: &mut JobCounter) -> io::Result<Self> {
        let args = template.split_for_job(jobs.next_job());
        let cmd = Self::new_command(&args.pre)?;

        Ok(Self {
            template,
            args,
            cmd,
            count: 0,
            limit,
            exit_code: ExitCode::Success,
        })
    }

    fn new_command(pre_args: &[OsString]) -> io::Result<Command> {
        let mut cmd = Command::new(&pre_args[0]);
        cmd.stdin(Stdio::inherit());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        cmd.try_args(&pre_args[1..])?;
        Ok(cmd)
    }

    fn push(
        &mut self,
        path: &Path,
        separator: Option<&str>,
        jobs: &mut JobCounter,
    ) -> io::Result<()> {
        if self.limit > 0 && self.count >= self.limit {
            self.finish(jobs)?;
        }

        let arg = self.args.path.generate(path, separator);
        if !self
            .cmd
            .args_would_fit(iter::once(&arg).chain(&self.args.post))
        {
            self.finish(jobs)?;
        }

        self.cmd.try_arg(arg)?;
        self.count += 1;
        Ok(())
    }

    fn finish(&mut self, jobs: &mut JobCounter) -> io::Result<()> {
        if self.count > 0 {
            self.cmd.try_args(&self.args.post)?;
            if !self.cmd.status()?.success() {
                self.exit_code = ExitCode::GeneralError;
            }

            self.args = self.template.split_for_job(jobs.next_job());
            self.cmd = Self::new_command(&self.args.pre)?;
            self.count = 0;
        }

        Ok(())
    }

    fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

/// Represents a template that is utilized to generate command strings.
///
/// The template is meant to be coupled with an input in order to generate a command. The
/// `generate_and_execute()` method will be used to generate a command and execute it.
#[derive(Debug, Clone, PartialEq)]
struct CommandTemplate {
    args: Vec<FormatTemplate>,
}

impl CommandTemplate {
    fn new<I, S>(input: I, mode: ExecutionMode) -> Result<CommandTemplate>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut args = Vec::new();
        let mut has_placeholder = false;

        for arg in input {
            let arg = arg.as_ref();

            let tmpl = FormatTemplate::parse(arg);
            // A job number is only meaningful when several results share one process.
            if mode == ExecutionMode::OneByOne && tmpl.has_job_number() {
                bail!("The '{{#}}' placeholder is only supported for --exec-batch");
            }
            has_placeholder |= tmpl.has_path_tokens();
            args.push(tmpl);
        }

        // We need to check that we have at least one argument, because if not
        // it will try to execute each file and directory it finds.
        //
        // Sadly, clap can't currently handle this for us, see
        // https://github.com/clap-rs/clap/issues/3542
        if args.is_empty() {
            bail!("No executable provided for --exec or --exec-batch");
        }

        // A placeholder as the executable is meaningful for `--exec` but never for `--exec-batch`.
        if mode == ExecutionMode::Batch && args[0].has_path_tokens() {
            bail!("First argument of --exec-batch must be a fixed executable, not a placeholder");
        }

        // If a placeholder token was not supplied, append one at the end of the command.
        if !has_placeholder {
            args.push(FormatTemplate::Tokens(vec![Token::Placeholder]));
        }

        Ok(CommandTemplate { args })
    }

    fn number_of_path_args(&self) -> usize {
        self.args.iter().filter(|arg| arg.has_path_tokens()).count()
    }

    /// Splits the arguments around the one holding the path placeholder, binding every
    /// job-number placeholder to `job`.
    ///
    /// The surrounding arguments no longer depend on the search result, so they are expanded
    /// right away; only the path argument is re-expanded for each entry in the batch.
    fn split_for_job(&self, job: usize) -> JobArgs {
        let mut pre = Vec::new();
        let mut path = None;
        let mut post = Vec::new();

        for arg in &self.args {
            let arg = arg.with_job_number(job);
            if arg.has_path_tokens() {
                path = Some(arg);
            } else if path.is_none() {
                pre.push(arg.generate("", None));
            } else {
                post.push(arg.generate("", None));
            }
        }

        JobArgs {
            pre,
            // `new()` appends an implicit placeholder when the template has none.
            path: path.expect("a batch template always holds a path placeholder"),
            post,
        }
    }

    /// Generates and executes a command.
    ///
    /// Using the internal `args` field, and a supplied `input` variable, a `Command` will be
    /// build.
    fn generate(&self, input: &Path, path_separator: Option<&str>) -> io::Result<Command> {
        let mut cmd = Command::new(self.args[0].generate(input, path_separator));
        for arg in &self.args[1..] {
            cmd.try_arg(arg.generate(input, path_separator))?;
        }
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_str(template: &CommandTemplate, input: &str) -> Vec<String> {
        template
            .args
            .iter()
            .map(|arg| arg.generate(input, None).into_string().unwrap())
            .collect()
    }

    #[test]
    fn tokens_with_placeholder() {
        assert_eq!(
            CommandSet::new(vec![vec![&"echo", &"${SHELL}:"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Text("${SHELL}:".into()),
                        FormatTemplate::Tokens(vec![Token::Placeholder]),
                    ]
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_with_no_extension() {
        assert_eq!(
            CommandSet::new(vec![vec!["echo", "{.}"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Tokens(vec![Token::NoExt]),
                    ],
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_with_basename() {
        assert_eq!(
            CommandSet::new(vec![vec!["echo", "{/}"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Tokens(vec![Token::Basename]),
                    ],
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_with_parent() {
        assert_eq!(
            CommandSet::new(vec![vec!["echo", "{//}"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Tokens(vec![Token::Parent]),
                    ],
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_with_basename_no_extension() {
        assert_eq!(
            CommandSet::new(vec![vec!["echo", "{/.}"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Tokens(vec![Token::BasenameNoExt]),
                    ],
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_with_literal_braces() {
        let template =
            CommandTemplate::new(vec!["{{}}", "{{", "{.}}"], ExecutionMode::OneByOne).unwrap();
        assert_eq!(
            generate_str(&template, "foo"),
            vec!["{}", "{", "{.}", "foo"]
        );
    }

    #[test]
    fn tokens_with_literal_braces_and_placeholder() {
        let template =
            CommandTemplate::new(vec!["echo", "{{{},end}"], ExecutionMode::OneByOne).unwrap();
        assert_eq!(generate_str(&template, "foo"), vec!["echo", "{foo,end}"]);
    }

    #[test]
    fn tokens_multiple() {
        assert_eq!(
            CommandSet::new(vec![vec!["cp", "{}", "{/.}.ext"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("cp".into()),
                        FormatTemplate::Tokens(vec![Token::Placeholder]),
                        FormatTemplate::Tokens(vec![
                            Token::BasenameNoExt,
                            Token::Text(".ext".into())
                        ]),
                    ],
                }],
                mode: ExecutionMode::OneByOne,
            }
        );
    }

    #[test]
    fn tokens_single_batch() {
        assert_eq!(
            CommandSet::new_batch(vec![vec!["echo", "{.}"]]).unwrap(),
            CommandSet {
                commands: vec![CommandTemplate {
                    args: vec![
                        FormatTemplate::Text("echo".into()),
                        FormatTemplate::Tokens(vec![Token::NoExt]),
                    ],
                }],
                mode: ExecutionMode::Batch,
            }
        );
    }

    #[test]
    fn tokens_multiple_batch() {
        assert!(CommandSet::new_batch(vec![vec!["echo", "{.}", "{}"]]).is_err());
    }

    #[test]
    fn template_no_args() {
        assert!(
            CommandTemplate::new::<Vec<_>, &'static str>(vec![], ExecutionMode::OneByOne).is_err()
        );
    }

    #[test]
    fn command_set_no_args() {
        assert!(CommandSet::new(vec![vec!["echo"], vec![]]).is_err());
    }

    #[test]
    fn tokens_job_number_batch_only() {
        assert!(CommandSet::new_batch(vec![vec!["echo", "{#}", "{}"]]).is_ok());
        // A job number is not a path placeholder, so the implicit "{}" is still appended...
        let template = CommandTemplate::new(vec!["echo", "{#}"], ExecutionMode::Batch).unwrap();
        assert_eq!(generate_str(&template, "foo"), vec!["echo", "{#}", "foo"]);
        // ...and it does not count towards the batch mode's single-placeholder limit.
        assert_eq!(template.number_of_path_args(), 1);

        assert!(CommandSet::new(vec![vec!["echo", "{#}"]]).is_err());
        assert!(CommandSet::new(vec![vec!["echo", "job{#}:", "{}"]]).is_err());
        // Escaped braces are literal text, not a job number.
        assert!(CommandSet::new(vec![vec!["echo", "{{#}}"]]).is_ok());
    }

    #[test]
    fn split_for_job_binds_job_number() {
        let template = CommandTemplate::new(
            vec!["cmd{#}", "-o", "out{#}", "{/}", "end{#}"],
            ExecutionMode::Batch,
        )
        .unwrap();

        let args = template.split_for_job(3);
        assert_eq!(args.pre, ["cmd3", "-o", "out3"]);
        assert_eq!(args.post, ["end3"]);
        assert_eq!(args.path.generate("dir/file.txt", None), "file.txt");

        // Every job re-binds the surrounding arguments.
        let args = template.split_for_job(4);
        assert_eq!(args.pre, ["cmd4", "-o", "out4"]);
        assert_eq!(args.post, ["end4"]);
    }

    #[test]
    fn job_counter_starts_at_one() {
        let mut jobs = JobCounter::default();
        assert_eq!(
            [jobs.next_job(), jobs.next_job(), jobs.next_job()],
            [1, 2, 3]
        );
    }

    #[test]
    fn placeholder_as_executable_rejected() {
        assert!(CommandSet::new(vec![vec!["{}"]]).is_ok());
        assert!(CommandSet::new(vec![vec!["{/}", "arg"]]).is_ok());
        assert!(CommandSet::new_batch(vec![vec!["{}"]]).is_err());
    }

    #[test]
    fn generate_custom_path_separator() {
        let arg = FormatTemplate::Tokens(vec![Token::Placeholder]);
        macro_rules! check {
            ($input:expr, $expected:expr) => {
                assert_eq!(arg.generate($input, Some("#")), OsString::from($expected));
            };
        }

        check!("foo", "foo");
        check!("foo/bar", "foo#bar");
        check!("/foo/bar/baz", "#foo#bar#baz");
    }

    #[cfg(windows)]
    #[test]
    fn generate_custom_path_separator_windows() {
        let arg = FormatTemplate::Tokens(vec![Token::Placeholder]);
        macro_rules! check {
            ($input:expr, $expected:expr) => {
                assert_eq!(arg.generate($input, Some("#")), OsString::from($expected));
            };
        }

        // path starting with a drive letter
        check!(r"C:\foo\bar", "C:#foo#bar");
        // UNC path
        check!(r"\\server\share\path", "##server#share#path");
        // Drive Relative path - no separator after the colon omits the RootDir path component.
        // This is uncommon, but valid
        check!(r"C:foo\bar", "C:foo#bar");

        // forward slashes should get normalized and interpreted as separators
        check!("C:/foo/bar", "C:#foo#bar");
        check!("C:foo/bar", "C:foo#bar");

        // Rust does not interpret "//server/share" as a UNC path, but rather as a normal
        // absolute path that begins with RootDir, and the two slashes get combined together as
        // a single path separator during normalization.
        //check!("//server/share/path", "##server#share#path");
    }
}
