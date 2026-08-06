//! Mini-helps: focused cheat-sheets printed when the literal value `help` is
//! passed to an option with non-obvious value syntax (a convention ported
//! from `f`). Deliberately not supported for `-m`/`-M`, where `help` is a
//! plausible matchset name; use `--list-matchsets` there instead.

use std::ffi::OsStr;

use clap::builder::{PossibleValue, TypedValueParser};

pub const TYPE: &str = "\
File types (for -t/--type):
  f  file             d  directory (dir)
  l  symlink          x  executable
  e  empty            s  socket
  p  pipe (FIFO)      b  block-device
  c  char-device

Long names work too (e.g. --type directory). Repeat the option to
combine types as OR: -tf -tl matches files and symlinks.
'x' matches regular files the current user can execute.
'e' matches empty files and directories; add 'f' or 'd' to narrow it.
";

pub const SIZE: &str = "\
Size filters (for -S/--size), format <+-><NUM><UNIT>:
  prefix: +  (at least)    -  (at most)    (none = exactly)
  units:  b  k  m  g  t    (base-10: 1k = 1000b)
          ki mi gi ti      (base-2:  1ki = 1024b)

Examples:
  -S +100k      at least 100 kilobytes
  -S -1m        at most 1 megabyte
  -S 4gi        exactly 4 gibibytes
  -S +1b        non-empty files
Repeat to combine: -S +1k -S -1m
";

pub const TIME: &str = "\
Time filters (for --changed-within/--changed-before):
  durations: 10h, 1d, 35min, 2weeks
  dates:     'YYYY-MM-DD', 'YYYY-MM-DD HH:MM:SS', '@unix_timestamp'

If a date's time of day is not specified, it defaults to 00:00:00.
";

pub const SORT: &str = "\
Sort expressions (for -R/--sort): a priority sequence of field letters,
evaluated left to right. Lowercase sorts ascending, uppercase descending.

  s/S  size                    m/M  modified time
  n/N  basename                c/C  changed time
  p/P  full path               a/A  accessed time
  e/E  extension               b/B  born time
  t/T  type (dir, file, ...)   i/I  inode
    z  case-insensitive collation for p/n/e
    Z  natural-number collation for p/n/e

Cannot be used with -x/-X. With -l, forces fd's internal listing.

Examples:
  -R mA   modified ascending, then accessed descending
  -R nZ   basename ascending with natural-number sorting
  -R P    full path descending
";

pub const EXEC: &str = "\
Command execution (-x/--exec, -X/--exec-batch):
  -x runs the command once per search result (in parallel).
  -X runs the command once with all results as arguments.

Placeholders:
  {}    path                {/}   basename
  {//}  parent directory    {.}   path without extension
  {/.}  basename without extension
  {#}   job number of the spawned process, 1-based (-X only)
  {{    literal '{'         }}    literal '}'

If no path placeholder is present, {} is appended implicitly.
All following arguments belong to the command; terminate the command
with ';' if more fd arguments follow.

-X spawns one process per batch, so {#} only varies when the results are
split up by --batch-size or by the command line length limit. Numbers are
unique across every process one fd run spawns.

Examples:
  fd -e zip -x unzip
  fd -e jpg -x convert {} {.}.png
  fd -e rs -X wc -l
  fd -e rs --batch-size 100 -X sh -c 'check \"$@\" > report{#}.txt' --

To run a program actually named 'help', use a path: -x ./help
";

pub const SUMMARIZE: &str = "\
Summary specs (for --summarize): '<summary>[:<options>]'.
  fext   count how many results share each file extension; a dotfile's
         whole name counts as its extension, and entries without an
         extension are counted under '(none)'

Options are single letters; prefix with '-' to disable an option or
'@' to restore its default. The last occurrence wins.
  i   fold case variations of an extension together
      (default: enabled on macOS and Windows, disabled elsewhere)
  d   include dotfiles (default: enabled)
  s   sort by ascending count; '-s' sorts by descending count

Examples: --summarize fext    --summarize fext:@d-i-s
";

pub const CONDEXP: &str = "\
Bash conditional expressions (for --bash, --prune-if, --exclude-if),
evaluated like bash's [[ ]] for each candidate entry.

Placeholder variables:
  ${}    path                ${/}   basename
  ${//}  parent directory    ${.}   path without extension
  ${/.}  basename without extension

Operators:
  file tests:  -e -f -d -h/-L -b -c -p -S -s(non-empty) -r -w -x
               -u -g -k -O -G -nt -ot -ef
  strings:     == / = (glob)  != (glob)  =~ (regex)  < > (lexicographic)
               -z (empty)  -n (non-empty)
  arithmetic:  -eq -ne -lt -le -gt -ge
  logic:       && || ! ( )

Relative paths in file tests resolve against the entry's context
directory: the directory itself for --prune-if, otherwise the entry's
parent directory.

Examples:
  --prune-if '${/} == target && -f CACHEDIR.TAG'
  --exclude-if '-x ${} && ${/} != *.sh'
  fd --bash '${/} == *.log && -s ${}'
";

/// Print `topic` to stdout and exit successfully, like `--help` does.
pub fn print_topic_and_exit(topic: &str) -> ! {
    print!("{topic}");
    std::process::exit(0);
}

/// A clap value parser that recognizes the literal value `help`, prints the
/// given topic, and exits; any other value is delegated to the inner parser.
/// Exiting inside a value parser is deliberate: it runs once, inside
/// `Opts::parse()`, exactly where `--help` itself exits.
#[derive(Clone)]
pub struct OrHelp<P> {
    inner: P,
    topic: &'static str,
}

impl<P> OrHelp<P> {
    pub fn new(inner: P, topic: &'static str) -> Self {
        Self { inner, topic }
    }
}

impl<P: TypedValueParser> TypedValueParser for OrHelp<P> {
    type Value = P::Value;

    fn parse_ref(
        &self,
        cmd: &clap::Command,
        arg: Option<&clap::Arg>,
        value: &OsStr,
    ) -> Result<Self::Value, clap::Error> {
        if value == "help" {
            print_topic_and_exit(self.topic);
        }
        self.inner.parse_ref(cmd, arg, value)
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        self.inner.possible_values()
    }
}
