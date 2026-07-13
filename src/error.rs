use std::io::IsTerminal;

use crate::sanitize::{maybe_sanitize, sanitize_multiline_for_terminal};

pub fn print_error(msg: impl Into<String>) {
    let msg = msg.into();
    let safe = maybe_sanitize(&msg, std::io::stderr().is_terminal());
    eprintln!("[fd error]: {safe}");
}

pub fn print_error_multiline(msg: impl Into<String>) {
    let msg = msg.into();
    let safe = if std::io::stderr().is_terminal() {
        sanitize_multiline_for_terminal(&msg)
    } else {
        msg.as_str().into()
    };
    eprintln!("[fd error]: {safe}");
}
