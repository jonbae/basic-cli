//! MVP, throwaway: hand-written codecs for the effect-ledger MVP wrappers.
//! Codegen would emit these from `Host.roc`.
//!
//! Each codec encodes what Roc sees, not the Rust-side value. An `io::Error`
//! becomes the `IOErr` tag `define_common_io_err!` would build from it: just
//! the tag for the nine named kinds, the message for `Other`. So decoding and
//! converting to Roc gives the value the recording handed to Roc.

use std::io;

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

// ---------------------------------------------------------------- display
//
// MVP 3, `ROC_LEDGER=dump:<dir>`: one display fn per wrapped function, in the
// terms of its `Host.roc` signature, so a dump shows the values Roc saw.
// `None`: this build has no codec for the symbol.

pub fn display_request(effect: &str, request: &[u8]) -> Option<String> {
    match effect {
        "hosted_stdout_line" | "hosted_stderr_line" => Some(roc_str(&String::from_utf8_lossy(request))),
        "hosted_stdin_line" | "hosted_utc_now" => Some(String::new()),
        "hosted_file_read_utf8" => Some(native_path(request)),
        _ => None,
    }
}

pub fn display_response(effect: &str, response: &[u8]) -> Option<String> {
    let unit = |tag: &str| match crate::decode_unit_response(response) {
        Ok(()) => "Ok({})".to_string(),
        Err(error) => format!("Err({tag}({}))", roc_io_err(&error)),
    };
    Some(match effect {
        "hosted_stdout_line" => unit("StdoutErr"),
        "hosted_stderr_line" => unit("StderrErr"),
        "hosted_stdin_line" => match crate::decode_stdin_line_response(response) {
            Ok(Some(line)) => format!("Ok({})", roc_str(&line)),
            Ok(None) => "Err(EndOfFile)".to_string(),
            Err(error) => format!("Err(StdinErr({}))", roc_io_err(&error)),
        },
        "hosted_file_read_utf8" => match decode_str_result(response) {
            Ok(content) => format!("Ok({})", roc_str(&content)),
            Err(error) => format!("Err(FileErr({}))", roc_io_err(&error)),
        },
        "hosted_utc_now" => match decode_utc_now(response) {
            Ok(nanos) => format!("Ok({nanos})  # {}", iso_utc(nanos)),
            Err(()) => "Err(ClockBeforeEpoch)".to_string(),
        },
        _ => return None,
    })
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
fn native_path(request: &[u8]) -> String {
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

/// Nanoseconds since the epoch as ISO 8601 UTC, to the microsecond.
pub fn iso_utc(nanos: u128) -> String {
    let secs = (nanos / 1_000_000_000) as i64;
    let micros = (nanos % 1_000_000_000) / 1000;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z", sod / 3600, sod % 3600 / 60, sod % 60)
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
        // io::Error isn't Clone: build each error twice.
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
    fn iso_utc_formats() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000000Z");
        // The MVP 1 crash: the app printed "Agenda at 2026-10-07T19:07:45Z".
        assert_eq!(iso_utc(1_791_400_065_828_387_000), "2026-10-07T19:07:45.828387Z");
        assert_eq!(iso_utc(951_782_400_000_000_000), "2000-02-29T00:00:00.000000Z");
    }

    #[test]
    fn display_shows_what_roc_saw() {
        let read = |r: io::Result<String>| display_response("hosted_file_read_utf8", &encode_str_result(&r)).unwrap();
        assert_eq!(read(Ok("a\n".into())), r#"Ok("a\n")"#);
        assert_eq!(read(Err(io::Error::from(io::ErrorKind::NotFound))), "Err(FileErr(NotFound))");
        assert_eq!(read(Err(io::Error::from_raw_os_error(libc::EIO))), r#"Err(FileErr(Other("Input/output error (os error 5)")))"#);
        assert_eq!(display_request("hosted_file_read_utf8", b"\0tasks.txt").unwrap(), r#"UnixBytes("tasks.txt")"#);
        assert_eq!(display_response("hosted_utc_now", &encode_utc_now(&Err(()))).unwrap(), "Err(ClockBeforeEpoch)");
        assert_eq!(display_response("hosted_stdout_line", &crate::encode_unit_response(&Err(io::Error::from_raw_os_error(libc::EPIPE)))).unwrap(), "Err(StdoutErr(BrokenPipe))");
        assert_eq!(display_response("hosted_file_delete", b""), None);
    }
}
