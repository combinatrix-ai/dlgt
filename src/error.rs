use std::fmt;

/// Public error codes. A code is attached where the error is raised and is
/// never recovered from message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InvalidArgument,
    NotFound,
    AliasInUse,
    SessionBusy,
    SessionBlocked,
    SessionUnavailable,
    SessionNotRunning,
    NoActiveExecution,
    LaunchFailed,
    CursorInvalid,
    CursorExpired,
    RequestNotFound,
    RequestResolved,
    RequestBusy,
    RequestFailed,
    RequestDeliveryFailed,
    CancelTimeout,
    AuthMismatch,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::NotFound => "NOT_FOUND",
            Self::AliasInUse => "ALIAS_IN_USE",
            Self::SessionBusy => "SESSION_BUSY",
            Self::SessionBlocked => "SESSION_BLOCKED",
            Self::SessionUnavailable => "SESSION_UNAVAILABLE",
            Self::SessionNotRunning => "SESSION_NOT_RUNNING",
            Self::NoActiveExecution => "NO_ACTIVE_EXECUTION",
            Self::LaunchFailed => "LAUNCH_FAILED",
            Self::CursorInvalid => "CURSOR_INVALID",
            Self::CursorExpired => "CURSOR_EXPIRED",
            Self::RequestNotFound => "REQUEST_NOT_FOUND",
            Self::RequestResolved => "REQUEST_RESOLVED",
            Self::RequestBusy => "REQUEST_BUSY",
            Self::RequestFailed => "REQUEST_FAILED",
            Self::RequestDeliveryFailed => "REQUEST_DELIVERY_FAILED",
            Self::CancelTimeout => "CANCEL_TIMEOUT",
            Self::AuthMismatch => "AUTH_MISMATCH",
        }
    }
}

#[derive(Debug)]
pub struct Coded {
    pub code: Code,
    message: String,
}

impl fmt::Display for Coded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

pub fn coded(code: Code, message: impl Into<String>) -> anyhow::Error {
    Coded {
        code,
        message: message.into(),
    }
    .into()
}

/// The code attached nearest to where the error was raised, if any.
pub fn code_of(error: &anyhow::Error) -> Option<Code> {
    error.chain().find_map(|cause| {
        if cause
            .downcast_ref::<crate::config::AuthMismatch>()
            .is_some()
        {
            Some(Code::AuthMismatch)
        } else {
            cause.downcast_ref::<Coded>().map(|coded| coded.code)
        }
    })
}

/// Return early with a coded error.
macro_rules! fail {
    ($code:ident, $($arg:tt)+) => {
        return Err($crate::error::coded($crate::error::Code::$code, format!($($arg)+)))
    };
}
pub(crate) use fail;

/// Attach a code to an error that has none yet.
pub trait WithCode<T> {
    fn code(self, code: Code) -> anyhow::Result<T>;
}

impl<T> WithCode<T> for anyhow::Result<T> {
    fn code(self, code: Code) -> anyhow::Result<T> {
        self.map_err(|error| with_code(error, code))
    }
}

pub fn with_code(error: anyhow::Error, code: Code) -> anyhow::Error {
    if code_of(&error).is_some() {
        error
    } else {
        coded(code, format!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{Code, WithCode, code_of, coded};

    #[test]
    fn a_code_survives_context_and_is_never_read_from_text() {
        let error = coded(Code::RequestBusy, "x").context("outer");
        assert_eq!(code_of(&error), Some(Code::RequestBusy));
        let text = anyhow::anyhow!("AUTH_MISMATCH REQUEST_FAILED launch not found");
        assert_eq!(code_of(&text), None);
    }

    #[test]
    fn with_code_keeps_the_innermost_code() {
        let error: anyhow::Result<()> = Err(coded(Code::CursorExpired, "old"));
        assert_eq!(
            error
                .code(Code::InvalidArgument)
                .err()
                .as_ref()
                .and_then(code_of),
            Some(Code::CursorExpired)
        );
    }
}
