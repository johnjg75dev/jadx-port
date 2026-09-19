//! Error model for the whole crate.
//!
//! jadx throws Java exceptions (`JadxDecompilerException`, `JadxInputException`,
//! `JavaClassParseException`, ...) out of the deep call stack. A shared library
//! must not unwind across the C ABI boundary, so every fallible operation here
//! returns [`Res`] and the FFI layer converts the error into an error code plus a
//! retrievable message (see `crates/jadx-ffi`).

use std::fmt;

/// Broad error category, mirrors the jadx exception hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
	/// Underlying file/IO failure (jadx: `IOException`).
	Io,
	/// Malformed or truncated input (jadx: `JavaClassParseException`,
	/// `JadxInputException`).
	InputFormat,
	/// Input is well formed but this port does not implement it yet.
	Unsupported,
	/// Caller passed a bad handle, index or argument.
	InvalidArgument,
	/// Requested class/member/entry does not exist.
	NotFound,
	/// Decompilation produced output but could not fully structure some method
	/// (jadx: "inconsistent code", still usable output).
	Incomplete,
	/// Bug in this crate.
	Internal,
}

impl ErrorKind {
	/// Stable numeric code used by the C ABI.
	pub fn code(self) -> i32 {
		match self {
			ErrorKind::Io => -1,
			ErrorKind::InputFormat => -2,
			ErrorKind::Unsupported => -3,
			ErrorKind::InvalidArgument => -4,
			ErrorKind::NotFound => -5,
			ErrorKind::Incomplete => -6,
			ErrorKind::Internal => -99,
		}
	}

	pub fn as_str(self) -> &'static str {
		match self {
			ErrorKind::Io => "io",
			ErrorKind::InputFormat => "input-format",
			ErrorKind::Unsupported => "unsupported",
			ErrorKind::InvalidArgument => "invalid-argument",
			ErrorKind::NotFound => "not-found",
			ErrorKind::Incomplete => "incomplete",
			ErrorKind::Internal => "internal",
		}
	}
}

/// Error value carried by [`Res`].
#[derive(Debug, Clone)]
pub struct JadxError {
	pub kind: ErrorKind,
	pub message: String,
}

impl JadxError {
	pub fn new<S: Into<String>>(kind: ErrorKind, message: S) -> Self {
		JadxError { kind, message: message.into() }
	}

	pub fn io<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::Io, message)
	}

	pub fn format<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::InputFormat, message)
	}

	pub fn unsupported<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::Unsupported, message)
	}

	pub fn not_found<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::NotFound, message)
	}

	pub fn invalid_argument<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::InvalidArgument, message)
	}

	pub fn internal<S: Into<String>>(message: S) -> Self {
		JadxError::new(ErrorKind::Internal, message)
	}

	/// Prefix the message with a `context` label, like jadx does when wrapping
	/// errors with the failing class/method ("in class: ..., method: ...").
	pub fn in_context<S: AsRef<str>>(mut self, context: S) -> Self {
		let ctx = context.as_ref();
		if !ctx.is_empty() {
			self.message = format!("{}: {}", ctx, self.message);
		}
		self
	}
}

impl fmt::Display for JadxError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}: {}", self.kind.as_str(), self.message)
	}
}

impl std::error::Error for JadxError {}

impl From<std::io::Error> for JadxError {
	fn from(e: std::io::Error) -> Self {
		JadxError::io(e.to_string())
	}
}

impl From<std::str::Utf8Error> for JadxError {
	fn from(e: std::str::Utf8Error) -> Self {
		JadxError::format(format!("invalid utf8: {}", e))
	}
}

impl From<fmt::Error> for JadxError {
	fn from(_: fmt::Error) -> Self {
		JadxError::internal("output buffer write failed")
	}
}

/// Crate-wide result alias.
pub type Res<T> = std::result::Result<T, JadxError>;

/// Helper used by the input loaders: keep the class/file name that failed so the
/// message is actionable, matching `JadxInputException.forFile(...)`.
pub fn wrap_file_context<T>(context: &str, res: Res<T>) -> Res<T> {
	match res {
		Ok(v) => Ok(v),
		Err(e) => Err(e.in_context(context)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn error_codes_are_stable() {
		assert_eq!(ErrorKind::Io.code(), -1);
		assert_eq!(ErrorKind::Unsupported.code(), -3);
		assert_eq!(ErrorKind::Internal.code(), -99);
	}

	#[test]
	fn context_is_prepended() {
		let e = JadxError::format("truncated").in_context("cls=A");
		assert_eq!(e.to_string(), "input-format: cls=A: truncated");
	}
}
