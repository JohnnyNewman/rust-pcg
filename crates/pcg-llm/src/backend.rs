//! The backend seam: one blocking call, text in, text out.
//!
//! Everything above this trait (what to ask, in which order, what to cache,
//! what to do with the answer) is backend-independent. A backend only turns a
//! [`Request`] into text, streaming it to a callback as it arrives.

use std::fmt;

/// How hard the model should think (maps to the API's `effort`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
    pub effort: Effort,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No credentials or a bad setting: nothing will work until it is fixed.
    Config(String),
    /// The server answered with an error status.
    Http {
        status: u16,
        message: String,
    },
    Network(String),
    /// The model (or a safety classifier) declined.
    Refused(Option<String>),
    /// Output hit `max_tokens` before it was complete.
    Truncated,
    /// The callback asked to stop.
    Cancelled,
    /// The answer could not be read.
    Protocol(String),
}

impl Error {
    /// Retrying (or asking about other nodes) cannot help.
    pub fn is_fatal(&self) -> bool {
        match self {
            Self::Config(_) | Self::Cancelled => true,
            Self::Http { status, .. } => matches!(status, 400 | 401 | 403 | 404),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(s) => write!(f, "{s}"),
            Self::Http { status, message } => write!(f, "HTTP {status}: {message}"),
            Self::Network(s) => write!(f, "network: {s}"),
            Self::Refused(Some(c)) => write!(f, "the model declined ({c})"),
            Self::Refused(None) => write!(f, "the model declined"),
            Self::Truncated => write!(f, "the answer was cut off (max_tokens)"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Protocol(s) => write!(f, "unreadable answer: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// A language model. Implementations are called from worker threads, possibly
/// several at once.
pub trait Backend: Send + Sync {
    /// Short name for the UI, e.g. the model id.
    fn name(&self) -> &str;
    /// Run `req` to completion. `on_text` sees the answer as it streams in;
    /// returning `false` from it cancels the request ([`Error::Cancelled`]).
    fn complete(&self, req: &Request, on_text: &mut dyn FnMut(&str) -> bool) -> Result<Completion, Error>;
}

/// A backend answering from a function: tests, demos, offline use.
pub struct Scripted<F>(pub F);

impl<F: Fn(&Request) -> Result<String, Error> + Send + Sync> Backend for Scripted<F> {
    fn name(&self) -> &str {
        "scripted"
    }
    fn complete(&self, req: &Request, on_text: &mut dyn FnMut(&str) -> bool) -> Result<Completion, Error> {
        let text = (self.0)(req)?;
        if !on_text(&text) {
            return Err(Error::Cancelled);
        }
        Ok(Completion { output_tokens: text.len() as u64 / 4, input_tokens: req.prompt.len() as u64 / 4, text })
    }
}
