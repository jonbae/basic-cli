//! PROTOTYPE, throwaway: basic-cli's binding to the effect-ledger library.
//! Codegen would emit this from `Host.roc`: one codec and one display per
//! wrapped hosted function, and the boundary table the library reads.
//!
//! Each codec encodes what Roc sees, not the Rust-side value. An `io::Error`
//! becomes the `IOErr` tag `define_common_io_err!` would build from it: just
//! the tag for the nine named kinds, the message for `Other`. So decoding and
//! converting to Roc gives the value the recording handed to Roc.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use effect_ledger::{iso_utc, Boundary, Echo, Entry, Hosted};

pub static BOUNDARY: Boundary = Boundary {
    platform: "basic-cli 0.23.0",
    hosted: &[
        Hosted { symbol: "hosted_stdout_line", echo: Echo::Stdout, show_request: show_line, show_response: |r| show_unit(r, "StdoutErr") },
        Hosted { symbol: "hosted_stderr_line", echo: Echo::Stderr, show_request: show_line, show_response: |r| show_unit(r, "StderrErr") },
        Hosted { symbol: "hosted_stdin_line", echo: Echo::Nothing, show_request: show_nothing, show_response: show_stdin_line },
        Hosted { symbol: "hosted_file_read_utf8", echo: Echo::Nothing, show_request: show_native_path, show_response: show_str_result },
        Hosted { symbol: "hosted_utc_now", echo: Echo::Nothing, show_request: show_nothing, show_response: show_utc_now },
    ],
    entries: &[Entry {
        name: "main!",
        after: &[],
        replay: |request| crate::run_main(decode_args(request).expect("main! request is not process args")).to_le_bytes().to_vec(),
        ok: |response| exit_code(response) == 0,
        show_request: show_args,
        show_response: |response| exit_code(response).to_string(),
    }],
};

// ---------------------------------------------------------------- main!

pub fn encode_args(args: &[OsString]) -> Vec<u8> {
    let mut b = Vec::new();
    for a in args {
        b.extend_from_slice(&(a.len() as u32).to_le_bytes());
        b.extend_from_slice(a.as_bytes());
    }
    b
}

fn decode_args(mut b: &[u8]) -> Option<Vec<OsString>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = u32::from_le_bytes(b.get(0..4)?.try_into().unwrap()) as usize;
        out.push(OsString::from_vec(b.get(4..4 + n)?.to_vec()));
        b = &b[4 + n..];
    }
    Some(out)
}

fn exit_code(response: &[u8]) -> i32 {
    i32::from_le_bytes(response[0..4].try_into().unwrap())
}

fn show_args(request: &[u8]) -> String {
    decode_args(request).map_or("unreadable".into(), |a| format!("{:?}", a.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>()))
}

// ---------------------------------------------------------------- codecs

// IOErr tags with no payload, in a fixed order: the byte is the index.
const IO_KINDS: [io::ErrorKind; 9] = [
    io::ErrorKind::AlreadyExists,
    io::ErrorKind::BrokenPipe,
    io::ErrorKind::Interrupted,
    io::ErrorKind::IsADirectory,
    io::ErrorKind::NotFound,
    io::ErrorKind::NotADirectory,
    io::ErrorKind::OutOfMemory,
    io::ErrorKind::PermissionDenied,
    io::ErrorKind::Unsupported,
];
// `Other(Str)`: every other kind, carrying `error.to_string()`.
const IO_OTHER: u8 = 255;

pub fn encode_io_err(error: &io::Error) -> Vec<u8> {
    match IO_KINDS.iter().position(|kind| *kind == error.kind()) {
        Some(index) => vec![index as u8],
        None => [&[IO_OTHER][..], error.to_string().as_bytes()].concat(),
    }
}

pub fn decode_io_err(bytes: &[u8]) -> io::Error {
    match bytes[0] {
        IO_OTHER => io::Error::other(String::from_utf8(bytes[1..].to_vec()).unwrap()),
        index => io::Error::from(IO_KINDS[index as usize]),
    }
}

/// `hosted_file_read_utf8` response: 0 + content | 1 + IOErr.
pub fn encode_str_result(result: &io::Result<String>) -> Vec<u8> {
    match result {
        Ok(content) => [&[0u8][..], content.as_bytes()].concat(),
        Err(error) => [&[1u8][..], &encode_io_err(error)].concat(),
    }
}

pub fn decode_str_result(bytes: &[u8]) -> io::Result<String> {
    match bytes[0] {
        0 => Ok(String::from_utf8(bytes[1..].to_vec()).unwrap()),
        _ => Err(decode_io_err(&bytes[1..])),
    }
}

/// `hosted_utc_now` response: 0 + nanos (u128 LE) | 1 (ClockBeforeEpoch).
pub fn encode_utc_now(result: &Result<u128, ()>) -> Vec<u8> {
    match result {
        Ok(nanos) => [&[0u8][..], &nanos.to_le_bytes()].concat(),
        Err(()) => vec![1],
    }
}

pub fn decode_utc_now(bytes: &[u8]) -> Result<u128, ()> {
    match bytes[0] {
        0 => Ok(u128::from_le_bytes(bytes[1..17].try_into().unwrap())),
        _ => Err(()),
    }
}

/// `hosted_stdout_line`/`hosted_stderr_line` response: 0 | 1 + raw os error (i32 LE).
pub fn encode_unit_response(result: &io::Result<()>) -> Vec<u8> {
    match result {
        Ok(()) => vec![0],
        Err(e) => [&[1u8][..], &e.raw_os_error().unwrap_or(0).to_le_bytes()].concat(),
    }
}

pub fn decode_unit_response(response: &[u8]) -> io::Result<()> {
    match response[0] {
        0 => Ok(()),
        _ => Err(io::Error::from_raw_os_error(i32::from_le_bytes(response[1..5].try_into().unwrap()))),
    }
}

/// `hosted_stdin_line` response: 0 + line | 1 (EOF) | 2 + raw os error (i32 LE).
pub fn encode_stdin_line_response(result: &io::Result<Option<String>>) -> Vec<u8> {
    match result {
        Ok(Some(line)) => [&[0u8][..], line.as_bytes()].concat(),
        Ok(None) => vec![1],
        Err(e) => [&[2u8][..], &e.raw_os_error().unwrap_or(0).to_le_bytes()].concat(),
    }
}

pub fn decode_stdin_line_response(response: &[u8]) -> io::Result<Option<String>> {
    match response[0] {
        0 => Ok(Some(String::from_utf8_lossy(&response[1..]).into_owned())),
        1 => Ok(None),
        _ => Err(io::Error::from_raw_os_error(i32::from_le_bytes(response[1..5].try_into().unwrap()))),
    }
}

// ---------------------------------------------------------------- display
//
// In the terms of each function's `Host.roc` signature, so a dump shows the
// values Roc saw.

fn show_nothing(_: &[u8]) -> String {
    String::new()
}

fn show_line(request: &[u8]) -> String {
    roc_str(&String::from_utf8_lossy(request))
}

fn show_unit(response: &[u8], tag: &str) -> String {
    match decode_unit_response(response) {
        Ok(()) => "Ok({})".to_string(),
        Err(error) => format!("Err({tag}({}))", roc_io_err(&error)),
    }
}

fn show_stdin_line(response: &[u8]) -> String {
    match decode_stdin_line_response(response) {
        Ok(Some(line)) => format!("Ok({})", roc_str(&line)),
        Ok(None) => "Err(EndOfFile)".to_string(),
        Err(error) => format!("Err(StdinErr({}))", roc_io_err(&error)),
    }
}

fn show_str_result(response: &[u8]) -> String {
    match decode_str_result(response) {
        Ok(content) => format!("Ok({})", roc_str(&content)),
        Err(error) => format!("Err(FileErr({}))", roc_io_err(&error)),
    }
}

fn show_utc_now(response: &[u8]) -> String {
    match decode_utc_now(response) {
        Ok(nanos) => format!("Ok({nanos})  # {}", iso_utc(nanos)),
        Err(()) => "Err(ClockBeforeEpoch)".to_string(),
    }
}

fn roc_str(s: &str) -> String {
    format!("{s:?}")
}

fn roc_io_err(error: &io::Error) -> String {
    match IO_KINDS.iter().find(|kind| **kind == error.kind()) {
        Some(kind) => format!("{kind:?}"),
        None => format!("Other({})", roc_str(&error.to_string())),
    }
}

// `NativePath`: tag byte, then its bytes. Bytes show as a string when they're UTF-8.
fn show_native_path(request: &[u8]) -> String {
    let body = &request[1..];
    match request[0] {
        0 => match std::str::from_utf8(body) {
            Ok(s) => format!("UnixBytes({})", roc_str(s)),
            Err(_) => format!("UnixBytes({body:?})"),
        },
        1 => format!("Utf8({})", roc_str(&String::from_utf8_lossy(body))),
        _ => {
            let units: Vec<u16> = body.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
            format!("WindowsU16s({})", roc_str(&String::from_utf16_lossy(&units)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What Roc sees of an io::Error: the named tag, or Other's message.
    fn roc_view(error: &io::Error) -> Result<io::ErrorKind, String> {
        if IO_KINDS.contains(&error.kind()) {
            Ok(error.kind())
        } else {
            Err(error.to_string())
        }
    }

    fn other_errors() -> Vec<io::Error> {
        vec![
            io::Error::from_raw_os_error(libc::EIO),
            io::Error::from_raw_os_error(libc::ENOSPC),
            io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"),
            io::Error::other("custom ünïcode message"),
            io::Error::other(""),
        ]
    }

    fn hosted(symbol: &str) -> &'static Hosted {
        BOUNDARY.hosted.iter().find(|h| h.symbol == symbol).unwrap()
    }

    #[test]
    fn every_named_io_err_tag_round_trips() {
        for kind in IO_KINDS {
            let error = io::Error::new(kind, "message Roc never sees");
            let decoded = decode_io_err(&encode_io_err(&error));
            assert_eq!(roc_view(&decoded), roc_view(&error));
        }
    }

    #[test]
    fn other_io_err_keeps_its_message() {
        for error in other_errors() {
            let decoded = decode_io_err(&encode_io_err(&error));
            assert_eq!(roc_view(&decoded), roc_view(&error));
        }
    }

    #[test]
    fn os_errors_map_to_the_same_tag() {
        for (errno, kind) in [(libc::ENOENT, io::ErrorKind::NotFound), (libc::EACCES, io::ErrorKind::PermissionDenied), (libc::EISDIR, io::ErrorKind::IsADirectory)] {
            let decoded = decode_io_err(&encode_io_err(&io::Error::from_raw_os_error(errno)));
            assert_eq!(roc_view(&decoded), Ok(kind));
        }
    }

    #[test]
    fn str_result_round_trips() {
        for content in ["", "09:00 Standup\n17:30 Submit essay draft\n", "ünïcode\r\n\0nul"] {
            let decoded = decode_str_result(&encode_str_result(&Ok(content.to_string()))).unwrap();
            assert_eq!(decoded, content);
        }
        let errors = || other_errors().into_iter().chain(IO_KINDS.map(io::Error::from));
        for (error, expected) in errors().zip(errors()) {
            let decoded = decode_str_result(&encode_str_result(&Err(error))).unwrap_err();
            assert_eq!(roc_view(&decoded), roc_view(&expected));
        }
    }

    #[test]
    fn utc_now_round_trips() {
        for result in [Ok(0), Ok(1_791_394_168_000_000_000), Ok(u128::MAX), Err(())] {
            assert_eq!(decode_utc_now(&encode_utc_now(&result)), result);
        }
    }

    #[test]
    fn args_round_trip() {
        let args: Vec<OsString> = vec!["./agenda".into(), "tasks.txt".into(), "".into()];
        assert_eq!(decode_args(&encode_args(&args)).unwrap(), args);
    }

    #[test]
    fn display_shows_what_roc_saw() {
        let read = |r: io::Result<String>| (hosted("hosted_file_read_utf8").show_response)(&encode_str_result(&r));
        assert_eq!(read(Ok("a\n".into())), r#"Ok("a\n")"#);
        assert_eq!(read(Err(io::Error::from(io::ErrorKind::NotFound))), "Err(FileErr(NotFound))");
        assert_eq!(read(Err(io::Error::from_raw_os_error(libc::EIO))), r#"Err(FileErr(Other("Input/output error (os error 5)")))"#);
        assert_eq!((hosted("hosted_file_read_utf8").show_request)(b"\0tasks.txt"), r#"UnixBytes("tasks.txt")"#);
        assert_eq!((hosted("hosted_utc_now").show_response)(&encode_utc_now(&Err(()))), "Err(ClockBeforeEpoch)");
        assert_eq!((hosted("hosted_stdout_line").show_response)(&encode_unit_response(&Err(io::Error::from_raw_os_error(libc::EPIPE)))), "Err(StdoutErr(BrokenPipe))");
        assert_eq!((BOUNDARY.entries[0].show_request)(&encode_args(&["./agenda".into(), "tasks.txt".into()])), r#"["./agenda", "tasks.txt"]"#);
    }
}
