// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use thiserror::Error as ThisError;
pub const GATHER_OS_INFO_CMD: &str = r#"sh -c '. /etc/os-release 2>/dev/null || . /usr/lib/os-release 2>/dev/null; printf "%s\n" "${ID:-unknown}" "${VERSION_ID:-${BUILD_ID:-unknown}}" "$(uname -r)"'"#;

/// Holds distro id, distro version, and kernel version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistroInfo {
    pub id: String,
    pub version: String,
    pub kernel: String,
}

#[derive(Debug, ThisError)]
pub enum ParseDistroError {
    #[error("expected {expected} lines, found {found}")]
    NotEnoughLines { expected: usize, found: usize },

    #[error("expected {expected} lines, found {found}")]
    TooManyLines { expected: usize, found: usize },
}

/// Parse the output of the three-line shell command into `DistroInfo`.
///
/// - Accepts optional trailing newlines and CRLF (`\r\n`)
/// - Ignores blank lines
pub fn parse_distro_info(input: &str) -> Result<DistroInfo, ParseDistroError> {
    const EXPECTED: usize = 3;

    let lines: Vec<String> = input
        .lines()
        .map(|l| l.trim_end_matches('\r').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    match lines.len() {
        n if n < EXPECTED => Err(ParseDistroError::NotEnoughLines {
            expected: EXPECTED,
            found: n,
        }),
        n if n > EXPECTED => Err(ParseDistroError::TooManyLines {
            expected: EXPECTED,
            found: n,
        }),
        _ => Ok(DistroInfo {
            id: lines[0].clone(),
            version: lines[1].clone(),
            kernel: lines[2].clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok() {
        let s = "ubuntu\n22.04\n6.8.0-45-generic\n";
        let got = parse_distro_info(s).unwrap();
        assert_eq!(
            got,
            DistroInfo {
                id: "ubuntu".into(),
                version: "22.04".into(),
                kernel: "6.8.0-45-generic".into()
            }
        );
    }

    #[test]
    fn tolerates_blank_and_crlf() {
        let s = "arch\r\nrolling\r\n6.9.7-arch1-1\r\n\r\n";
        let got = parse_distro_info(s).unwrap();
        assert_eq!(got.id, "arch");
        assert_eq!(got.version, "rolling");
        assert_eq!(got.kernel, "6.9.7-arch1-1");
    }

    #[test]
    fn errors_on_too_few_lines() {
        let s = "ubuntu\n22.04\n";
        let err = parse_distro_info(s).unwrap_err();
        match err {
            ParseDistroError::NotEnoughLines { expected, found } => {
                assert_eq!(expected, 3);
                assert_eq!(found, 2);
            }
            _ => panic!("unexpected error variant"),
        }
    }

    #[test]
    fn errors_on_too_many_lines() {
        let s = "ubuntu\n22.04\n6.8.0-45-generic\nextra\n";
        let err = parse_distro_info(s).unwrap_err();
        match err {
            ParseDistroError::TooManyLines { expected, found } => {
                assert_eq!(expected, 3);
                assert_eq!(found, 4);
            }
            _ => panic!("unexpected error variant"),
        }
    }
}
