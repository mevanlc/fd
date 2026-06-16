use std::borrow::Cow;
use std::fs::Metadata;
use std::io::{self, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use nix::unistd::{Gid, Group, Uid, User};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use lscolors::{Indicator, LsColors, Style};

use jiff::Zoned;
use jiff::fmt::StdIoWrite;
use jiff::fmt::strtime::{BrokenDownTime, Config as TimeFormatConfig};

use crate::config::Config;
use crate::dir_entry::DirEntry;
use crate::filesystem;
use crate::fmt::FormatTemplate;
use crate::hyperlink::PathUrl;

const SECS_PER_AVERAGE_GREGORIAN_YEAR: u64 = 31_556_952;
const RECENT_TIME_FORMAT: &str = "%b %e %H:%M";
const OLDER_TIME_FORMAT: &str = "%b %e  %Y";

fn replace_path_separator(path: &str, new_path_separator: &str) -> String {
    path.replace(std::path::MAIN_SEPARATOR, new_path_separator)
}

// TODO: this function is performance critical and can probably be optimized
pub fn print_entry<W: Write>(stdout: &mut W, entry: &DirEntry, config: &Config) -> io::Result<()> {
    let mut has_hyperlink = false;
    if config.hyperlink
        && let Some(url) = PathUrl::new(entry.path())
    {
        write!(stdout, "\x1B]8;;{url}\x1B\\")?;
        has_hyperlink = true;
    }

    if config.list_details {
        print_entry_list_details(stdout, entry, config)?;
    } else if let Some(ref format) = config.format {
        print_entry_format(stdout, entry, config, format)?;
    } else if let Some(ref ls_colors) = config.ls_colors {
        print_entry_colorized(stdout, entry, config, ls_colors)?;
    } else {
        print_entry_uncolorized(stdout, entry, config)?;
    };

    if has_hyperlink {
        write!(stdout, "\x1B]8;;\x1B\\")?;
    }

    if config.null_separator {
        write!(stdout, "\0")
    } else {
        writeln!(stdout)
    }
}

fn print_entry_list_details<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
) -> io::Result<()> {
    let file_type = file_type_char(entry);
    let permissions = entry
        .metadata()
        .map(format_permissions)
        .unwrap_or_else(|| "---------".to_string());
    let links = entry.metadata().map(number_of_links).unwrap_or(0);
    let (owner, group) = entry
        .metadata()
        .map(owner_group)
        .unwrap_or_else(|| (String::from("-"), String::from("-")));
    let size = entry
        .metadata()
        .map(|m| format_human_size(m.len()))
        .unwrap_or_else(|| "?".to_string());
    let modified = entry
        .metadata()
        .map(format_modified_timestamp)
        .unwrap_or_else(|| "-".to_string());
    let path = format_path_with_symlink_target(entry, config);

    write!(
        stdout,
        "{file_type}{permissions} {links:>2} {owner:<8} {group:<8} {size:>5} {modified} {path}"
    )
}

fn format_path_with_symlink_target(entry: &DirEntry, config: &Config) -> String {
    let mut path = entry.stripped_path(config).to_string_lossy().to_string();
    if let Some(ref separator) = config.path_separator {
        path = replace_path_separator(&path, separator);
    }

    if entry.file_type().is_some_and(|ft| ft.is_dir()) {
        path.push_str(&config.actual_path_separator);
    }

    if entry.file_type().is_some_and(|ft| ft.is_symlink())
        && let Ok(target) = std::fs::read_link(entry.path())
    {
        let mut target = target.to_string_lossy().to_string();
        if let Some(ref separator) = config.path_separator {
            target = replace_path_separator(&target, separator);
        }
        path.push_str(" -> ");
        path.push_str(&target);
    }

    path
}

fn file_type_char(entry: &DirEntry) -> char {
    if let Some(ft) = entry.file_type() {
        if ft.is_dir() {
            return 'd';
        }
        if ft.is_symlink() {
            return 'l';
        }
        if filesystem::is_block_device(ft) {
            return 'b';
        }
        if filesystem::is_char_device(ft) {
            return 'c';
        }
        if filesystem::is_socket(ft) {
            return 's';
        }
        if filesystem::is_pipe(ft) {
            return 'p';
        }
        if ft.is_file() {
            return '-';
        }
    }

    '?'
}

#[cfg(unix)]
fn format_permissions(metadata: &Metadata) -> String {
    let mode = metadata.permissions().mode();
    let mut chars = ['-'; 9];
    for (idx, (bit, ch)) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ]
    .iter()
    .enumerate()
    {
        if mode & bit != 0 {
            chars[idx] = *ch;
        }
    }
    chars.iter().collect()
}

#[cfg(windows)]
fn format_permissions(metadata: &Metadata) -> String {
    if metadata.permissions().readonly() {
        "r--r--r--".to_string()
    } else {
        "rw-rw-rw-".to_string()
    }
}

#[cfg(unix)]
fn number_of_links(metadata: &Metadata) -> u64 {
    metadata.nlink()
}

#[cfg(windows)]
fn number_of_links(metadata: &Metadata) -> u64 {
    metadata.number_of_links()
}

#[cfg(unix)]
fn owner_group(metadata: &Metadata) -> (String, String) {
    let uid = Uid::from_raw(metadata.uid());
    let gid = Gid::from_raw(metadata.gid());

    let owner = User::from_uid(uid)
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.as_raw().to_string());
    let group = Group::from_gid(gid)
        .ok()
        .flatten()
        .map(|g| g.name)
        .unwrap_or_else(|| gid.as_raw().to_string());

    (owner, group)
}

#[cfg(windows)]
fn owner_group(_: &Metadata) -> (String, String) {
    (String::from("-"), String::from("-"))
}

fn format_modified_timestamp(metadata: &Metadata) -> String {
    metadata
        .modified()
        .ok()
        .map(format_list_details_timestamp)
        .unwrap_or_else(|| "-".to_string())
}

fn format_list_details_timestamp(time: SystemTime) -> String {
    format_list_details_timestamp_with_now(time, SystemTime::now())
}

fn format_list_details_timestamp_with_now(time: SystemTime, now: SystemTime) -> String {
    let recent_start = now - Duration::from_secs(SECS_PER_AVERAGE_GREGORIAN_YEAR / 2);
    let fmt = if (recent_start..=now).contains(&time) {
        RECENT_TIME_FORMAT
    } else {
        OLDER_TIME_FORMAT
    };

    format_system_time(time, fmt).unwrap_or_else(|| format_epoch_seconds(time))
}

fn format_system_time(time: SystemTime, fmt: &str) -> Option<String> {
    let zoned = Zoned::try_from(time).ok()?;
    let time = BrokenDownTime::from(&zoned);
    let mut out = Vec::new();
    let mut writer = StdIoWrite(&mut out);
    let config = TimeFormatConfig::new().lenient(true);
    time.format_with_config(&config, fmt, &mut writer).ok()?;
    String::from_utf8(out).ok()
}

fn format_epoch_seconds(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(error) => format!("-{}", error.duration().as_secs()),
    }
}

fn format_human_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P"];
    if size < 1024 {
        return size.to_string();
    }

    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

// Display a trailing slash if the path is a directory and the config option is enabled.
// If the path_separator option is set, display that instead.
// The trailing slash will not be colored.
#[inline]
fn print_trailing_slash<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
    style: Option<&Style>,
) -> io::Result<()> {
    if entry.file_type().is_some_and(|ft| ft.is_dir()) {
        write!(
            stdout,
            "{}",
            style
                .map(Style::to_nu_ansi_term_style)
                .unwrap_or_default()
                .paint(&config.actual_path_separator)
        )?;
    }
    Ok(())
}

// TODO: this function is performance critical and can probably be optimized
fn print_entry_format<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
    format: &FormatTemplate,
) -> io::Result<()> {
    let output = format.generate(
        entry.stripped_path(config),
        config.path_separator.as_deref(),
    );
    // TODO: support writing raw bytes on unix?
    write!(stdout, "{}", output.to_string_lossy())
}

// TODO: this function is performance critical and can probably be optimized
fn print_entry_colorized<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
    ls_colors: &LsColors,
) -> io::Result<()> {
    // Split the path between the parent and the last component
    let mut offset = 0;
    let path = entry.stripped_path(config);
    let path_str = path.to_string_lossy();

    if let Some(parent) = path.parent() {
        offset = parent.to_string_lossy().len();
        for c in path_str[offset..].chars() {
            if std::path::is_separator(c) {
                offset += c.len_utf8();
            } else {
                break;
            }
        }
    }

    if offset > 0 {
        let mut parent_str = Cow::from(&path_str[..offset]);
        if let Some(ref separator) = config.path_separator {
            *parent_str.to_mut() = replace_path_separator(&parent_str, separator);
        }

        let style = ls_colors
            .style_for_indicator(Indicator::Directory)
            .map(Style::to_nu_ansi_term_style)
            .unwrap_or_default();
        write!(stdout, "{}", style.paint(parent_str))?;
    }

    let style = entry
        .style(ls_colors)
        .map(Style::to_nu_ansi_term_style)
        .unwrap_or_default();
    write!(stdout, "{}", style.paint(&path_str[offset..]))?;

    print_trailing_slash(
        stdout,
        entry,
        config,
        ls_colors.style_for_indicator(Indicator::Directory),
    )?;

    Ok(())
}

// TODO: this function is performance critical and can probably be optimized
fn print_entry_uncolorized_base<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
) -> io::Result<()> {
    let path = entry.stripped_path(config);

    let mut path_string = path.to_string_lossy();
    if let Some(ref separator) = config.path_separator {
        *path_string.to_mut() = replace_path_separator(&path_string, separator);
    }
    write!(stdout, "{path_string}")?;
    print_trailing_slash(stdout, entry, config, None)
}

#[cfg(not(unix))]
fn print_entry_uncolorized<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
) -> io::Result<()> {
    print_entry_uncolorized_base(stdout, entry, config)
}

#[cfg(unix)]
fn print_entry_uncolorized<W: Write>(
    stdout: &mut W,
    entry: &DirEntry,
    config: &Config,
) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if config.interactive_terminal || config.path_separator.is_some() {
        // Fall back to the base implementation
        print_entry_uncolorized_base(stdout, entry, config)
    } else {
        // Print path as raw bytes, allowing invalid UTF-8 filenames to be passed to other processes
        stdout.write_all(entry.stripped_path(config).as_os_str().as_bytes())?;
        print_trailing_slash(stdout, entry, config, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_details_timestamp_uses_recent_format_for_recent_times() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let recent = now - Duration::from_secs(60);

        let formatted = format_list_details_timestamp_with_now(recent, now);

        assert!(
            formatted.contains(':'),
            "recent timestamp should include HH:MM: {formatted}"
        );
        assert!(
            !formatted.contains("2023"),
            "recent timestamp should not include the year: {formatted}"
        );
    }

    #[test]
    fn list_details_timestamp_uses_older_format_for_old_and_future_times() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let old = now - Duration::from_secs(SECS_PER_AVERAGE_GREGORIAN_YEAR);
        let future = now + Duration::from_secs(60);

        for timestamp in [old, future] {
            let formatted = format_list_details_timestamp_with_now(timestamp, now);

            assert!(
                !formatted.contains(':'),
                "older/future timestamp should not include HH:MM: {formatted}"
            );
        }
    }

    #[test]
    fn list_details_timestamp_falls_back_to_epoch_seconds() {
        let out_of_range = UNIX_EPOCH + Duration::from_secs(253_402_300_800);

        assert_eq!(
            "253402300800",
            format_list_details_timestamp_with_now(out_of_range, UNIX_EPOCH)
        );
    }
}
