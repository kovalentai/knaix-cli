//! What `knaix` returns to the shell.
//!
//! A script that calls the CLI needs to tell "the node said no" apart from "the
//! node was not there", and a single exit code of 1 for every failure makes that
//! impossible without parsing English error text. So every failure carries a
//! code, and the codes are part of the interface: they are documented in the
//! README, and changing what one means is a breaking change.
//!
//! Codes are attached in two ways. Most are stated at the point the error is
//! raised, with `.coded(Code::X)`. Transport failures are recognised from the
//! error chain instead, so every call that reaches the network reports
//! `Unavailable` without each of them having to remember to say so.
//!
//! One ending is not a code here. A reader that closes the pipe -- `| head`,
//! quitting a pager -- kills the process with SIGPIPE, which a shell reports as
//! 141. That is the Unix convention rather than anything of ours, and it is
//! deliberately not one of the codes below: nothing failed, the reader left.

use std::fmt;

/// The exit codes `knaix` returns.
///
/// Numbers are fixed. 0 and 2 follow the shell's conventions, and 2 belongs to
/// clap, which exits with it directly on a bad argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Code {
    /// The command did what was asked.
    Ok = 0,
    /// Something failed and no more specific code fits.
    Error = 1,
    /// The command line itself was wrong: unknown flag, missing argument.
    Usage = 2,
    /// Not logged in, or the credential was rejected.
    Auth = 3,
    /// A node or the control plane could not be reached.
    Unavailable = 4,
    /// The node, document, or thread named does not exist.
    NotFound = 5,
    /// Refused on purpose: a policy said no, or a confirmation was declined.
    Denied = 6,
    /// The machine is not ready: no local node running, Docker absent.
    Precondition = 7,
}

impl Code {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The code an HTTP status implies.
    ///
    /// Only the statuses that mean something different to a caller are mapped.
    /// A 500 is a plain failure: retrying is the same as retrying anything else.
    pub fn for_status(status: u16) -> Self {
        match status {
            401 | 403 => Code::Auth,
            404 => Code::NotFound,
            _ => Code::Error,
        }
    }
}

/// An error carrying the code it should exit with.
///
/// Display and `source` both delegate, so wrapping an error in this changes the
/// exit code and nothing the user sees.
#[derive(Debug)]
pub struct Coded {
    code: Code,
    inner: anyhow::Error,
}

impl fmt::Display for Coded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for Coded {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

/// Attach an exit code to a failure.
pub trait WithCode {
    /// Say what this failure should exit with. A code already attached deeper
    /// in the chain wins, since it was closer to what actually went wrong.
    fn coded(self, code: Code) -> Self;
}

impl<T> WithCode for anyhow::Result<T> {
    fn coded(self, code: Code) -> Self {
        self.map_err(|e| {
            if code_in_chain(&e).is_some() {
                return e;
            }
            anyhow::Error::new(Coded { code, inner: e })
        })
    }
}

fn code_in_chain(err: &anyhow::Error) -> Option<Code> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<Coded>().map(|c| c.code))
}

/// The code a failure exits with.
///
/// An explicit tag wins. Failing that, a transport error anywhere in the chain
/// means the far end was not reachable, which is worth distinguishing from the
/// far end refusing: the first is worth retrying and the second is not.
pub fn code_of(err: &anyhow::Error) -> Code {
    if let Some(code) = code_in_chain(err) {
        return code;
    }
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
            if e.is_connect() || e.is_timeout() {
                return Code::Unavailable;
            }
        }
    }
    Code::Error
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context, Result};

    fn err(msg: &str) -> Result<()> {
        Err(anyhow!("{}", msg))
    }

    #[test]
    fn numbers_are_fixed() {
        // These are interface. A script keys off them, so a change here is a
        // breaking change and this test is the reminder.
        assert_eq!(Code::Ok.as_u8(), 0);
        assert_eq!(Code::Error.as_u8(), 1);
        assert_eq!(Code::Usage.as_u8(), 2);
        assert_eq!(Code::Auth.as_u8(), 3);
        assert_eq!(Code::Unavailable.as_u8(), 4);
        assert_eq!(Code::NotFound.as_u8(), 5);
        assert_eq!(Code::Denied.as_u8(), 6);
        assert_eq!(Code::Precondition.as_u8(), 7);
    }

    #[test]
    fn an_untagged_failure_is_a_plain_error() {
        let e = err("something broke").unwrap_err();
        assert_eq!(code_of(&e), Code::Error);
    }

    #[test]
    fn a_tag_is_read_back() {
        let e = err("not logged in").coded(Code::Auth).unwrap_err();
        assert_eq!(code_of(&e), Code::Auth);
    }

    /// Tagging must not change a single character of what the user reads.
    #[test]
    fn tagging_leaves_the_message_alone() {
        let plain = err("could not read the node list").unwrap_err();
        let tagged = err("could not read the node list")
            .coded(Code::NotFound)
            .unwrap_err();
        assert_eq!(format!("{plain}"), format!("{tagged}"));
        assert_eq!(format!("{plain:#}"), format!("{tagged:#}"));
    }

    /// Context added above a tag must keep both the code and the full chain.
    #[test]
    fn context_added_later_keeps_the_code_and_the_chain() {
        let e = err("connection refused")
            .coded(Code::Unavailable)
            .context("Could not reach the node")
            .unwrap_err();
        assert_eq!(code_of(&e), Code::Unavailable);
        let rendered = format!("{e:#}");
        assert!(rendered.contains("Could not reach the node"), "{rendered}");
        assert!(rendered.contains("connection refused"), "{rendered}");
    }

    /// The inner code is the one that knows what actually failed.
    #[test]
    fn the_innermost_code_wins() {
        let e = err("no such node")
            .coded(Code::NotFound)
            .coded(Code::Error)
            .unwrap_err();
        assert_eq!(code_of(&e), Code::NotFound);
    }

    #[test]
    fn statuses_map_to_the_codes_a_caller_can_act_on() {
        assert_eq!(Code::for_status(401), Code::Auth);
        assert_eq!(Code::for_status(403), Code::Auth);
        assert_eq!(Code::for_status(404), Code::NotFound);
        // Nothing a caller can do differently, so it stays a plain failure.
        assert_eq!(Code::for_status(500), Code::Error);
        assert_eq!(Code::for_status(429), Code::Error);
    }
}
