#![forbid(unused_crate_dependencies)]

use std::{fmt, io};

pub use terminal::{Terminal, Termion};

pub mod server;
pub mod terminal;

/// Builds Assuan server that implements a pinentry-tty tool
///
/// Alias for wrapping [server::PinentryTty] into [pinentry::PinentryServer] and
/// converting into [assuan_server::AssuanServer].
///
/// ### Example
/// Launch a pinentry-tty server that accepts commands from stdin and writes responses
/// to stdout:
/// ```rust
#[doc = include_str!("main.rs")]
/// ```
pub fn server() -> assuan_server::AssuanServer<
    pinentry::PinentryServer<server::PinentryTty>,
    impl assuan_server::router::CmdList<pinentry::PinentryServer<server::PinentryTty>>,
> {
    pinentry::PinentryServer::new(server::PinentryTty::default()).build_assuan_server()
}

pub fn ask_pin(
    tty: &mut impl Terminal,
    prompt: impl fmt::Display,
    out: &mut impl PushPop<char>,
) -> Result<bool, AskPinError> {
    write!(tty, "{prompt}").map_err(AskPinError::Write)?;
    tty.flush().map_err(AskPinError::Write)?;

    if read_pin(tty, out)? {
        writeln!(tty).map_err(AskPinError::Write)?;
        Ok(true)
    } else {
        writeln!(tty, "Aborted.").map_err(AskPinError::Write)?;
        Ok(false)
    }
}

fn read_pin(tty: &mut impl Terminal, out: &mut impl PushPop<char>) -> Result<bool, AskPinError> {
    use terminal::Key;

    let (keys, _tty_out) = tty.keys().map_err(AskPinError::RawMode)?;
    for k in keys {
        match k.map_err(AskPinError::Read)? {
            Key::Char('\n') | Key::Char('\r') => return Ok(true),
            Key::Char(x) => {
                out.push(x).map_err(|_| AskPinError::PinTooLong)?;
            }
            Key::Backspace => {
                let _ = out.pop();
            }
            Key::Ctrl('c')
            | Key::Ctrl('C')
            | Key::Ctrl('d')
            | Key::Ctrl('D')
            | Key::Null
            | Key::Esc => return Ok(false),
            _ => continue,
        }
    }
    Err(AskPinError::Read(io::ErrorKind::UnexpectedEof.into()))
}

/// Container that provides push/pop access
///
/// The trait is used to store PIN typed by the user in [`ask_pin`], therefore the trait implementation
/// must treat its content as highly sensitive.
///
/// Out of box, we provide an implementation of the trait for the `Zeroizing<String>`:
/// 1. [`Zeroizing`](zeroize::Zeroizing) ensures that the PIN is erased from the memory when dropped
/// 2. Implementation does not allow the string to grow: `push` operation is only possible
///    if the string has some capacity left \
///    Growing the string leaves a partial copy of it on heap which is not desired for sensitive information.
///
/// ## Example
/// ```rust
/// use pinentry_tty::PushPop;
/// use zeroize::Zeroizing;
///
/// let mut buffer = Zeroizing::new(String::with_capacity(10));
/// for x in "0123456789".chars() {
///     buffer.push(x)?;
/// }
///
/// // Pushing any more character would require string to grow, so error is returned
/// buffer.push('a').unwrap_err();
/// # Ok::<_, char>(())
/// ```
pub trait PushPop<T> {
    /// Appends `x`
    ///
    /// Returns `Err(x)` if container cannot take it
    fn push(&mut self, x: T) -> Result<(), T>;
    /// Pops the last element
    fn pop(&mut self) -> Option<T>;
}

impl PushPop<char> for assuan_server::response::SecretData {
    fn push(&mut self, x: char) -> Result<(), char> {
        (**self).push(x).map_err(|_| x)
    }

    fn pop(&mut self) -> Option<char> {
        (**self).pop()
    }
}

/// Push/pop access to the string without reallocation
///
/// `push` operation will never cause the internal buffer of `String` to grow
impl PushPop<char> for zeroize::Zeroizing<String> {
    /// Appends a character to the string if it has free capacity
    ///
    /// ```rust
    /// use pinentry_tty::PushPop;
    /// use zeroize::Zeroizing;
    ///
    /// let mut buf = Zeroizing::new(String::with_capacity(2));
    /// buf.push('a').unwrap();
    /// buf.push('b').unwrap();
    ///
    /// // String has no internal capacity left. Pushing new element
    /// // will not succeed
    /// buf.push('c').unwrap_err();
    /// ```
    fn push(&mut self, x: char) -> Result<(), char> {
        if self.len() + x.len_utf8() <= self.capacity() {
            (**self).push(x);
            Ok(())
        } else {
            Err(x)
        }
    }

    fn pop(&mut self) -> Option<char> {
        (**self).pop()
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum AskPinError {
    Read(io::Error),
    Write(io::Error),
    RawMode(io::Error),
    PinTooLong,
}

impl fmt::Display for AskPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskPinError::Read(err) => write!(f, "read from tty: {err}"),
            AskPinError::Write(err) => write!(f, "write to tty: {err}"),
            AskPinError::RawMode(err) => write!(f, "switch to raw mode: {err}"),
            AskPinError::PinTooLong => write!(f, "pin is too long"),
        }
    }
}

pub fn dialog<'a, T>(
    tty: &mut impl Terminal,
    message: impl fmt::Display,
    options: &'a [(&str, T)],
) -> Result<Option<&'a T>, DialogError> {
    let options = options.iter().fold(
        Vec::with_capacity(options.len()),
        |mut acc, (text, value)| {
            let option = DialogOption::new(text, value, &acc);
            acc.push(option);
            acc
        },
    );

    writeln!(tty, "{message}").map_err(DialogError::Write)?;

    let result = render_options(tty, &options);
    writeln!(tty).map_err(DialogError::Write)?;
    result
}

struct DialogOption<'a, T> {
    text: &'a str,
    short: Option<char>,
    value: &'a T,
}

impl<'a, T> DialogOption<'a, T> {
    pub fn new(text: &'a str, value: &'a T, existing_options: &[DialogOption<T>]) -> Self {
        let short_already_used =
            |&short: &char| existing_options.iter().any(|o| Some(short) == o.short);
        let available_short = text
            .chars()
            .filter(|x| x.is_alphabetic())
            .find(|x| !short_already_used(x));

        Self {
            short: available_short,
            text,
            value,
        }
    }

    pub fn render(&self, tty_out: &mut impl std::io::Write) -> Result<(), DialogError> {
        if let Some(short) = self.short {
            use termion::style::{NoUnderline, Underline};
            let (left, right) = self
                .text
                .split_once(short)
                .ok_or(BugReason::ShortCharacterNotFound)?;
            write!(tty_out, "{left}{Underline}{short}{NoUnderline}{right}")
                .map_err(DialogError::Write)?;
        } else {
            write!(tty_out, "{}", self.text).map_err(DialogError::Write)?;
        }
        Ok(())
    }
}

fn render_options<'a, T>(
    tty: &mut impl Terminal,
    options: &[DialogOption<'a, T>],
) -> Result<Option<&'a T>, DialogError> {
    use std::io::Write;
    use terminal::Key;
    use termion::style::{NoUnderline, Underline};

    if options.len() > 9 {
        return Err(DialogError::TooManyOptions);
    }

    for (i, option) in (1..).zip(options) {
        write!(tty, "  {Underline}{i}{NoUnderline} ").map_err(DialogError::Write)?;
        option.render(tty)?;
        writeln!(tty).map_err(DialogError::Write)?;
    }

    write!(tty, "Type [").map_err(DialogError::Write)?;
    for i in 1..=options.len() {
        write!(tty, "{i}").map_err(DialogError::Write)?;
    }
    for short in options
        .iter()
        .flat_map(|o| o.short)
        .map(|s| s.to_lowercase())
    {
        write!(tty, "{short}").map_err(DialogError::Write)?;
    }
    write!(tty, "] : ").map_err(DialogError::Write)?;
    tty.flush().map_err(DialogError::Write)?;

    let (keys, mut tty_out) = tty.keys().map_err(DialogError::RawMode)?;

    for key in keys {
        tty_out.flush().map_err(DialogError::Write)?;
        match key.map_err(DialogError::Read)? {
            Key::Char(x) => {
                if let Some(index) = x.to_digit(10) {
                    let Ok(index): Result<usize, _> = index.try_into() else {
                        continue;
                    };
                    let Some(index) = index.checked_sub(1) else {
                        continue;
                    };
                    let Some(option) = options.get(index) else {
                        continue;
                    };
                    write!(tty_out, "{}", x).map_err(DialogError::Write)?;
                    return Ok(Some(option.value));
                } else {
                    let Some(option) = options.iter().find(|o| {
                        o.short
                            .map(|s| s.to_lowercase().eq(x.to_lowercase()))
                            .unwrap_or(false)
                    }) else {
                        continue;
                    };
                    write!(tty_out, "{}", x).map_err(DialogError::Write)?;
                    return Ok(Some(option.value));
                }
            }
            Key::Ctrl('c' | 'C' | 'd' | 'D') | Key::Null | Key::Esc => {
                write!(tty_out, "Aborted.").map_err(DialogError::Write)?;
                return Ok(None);
            }
            _ => {
                // ignore
            }
        }
    }
    Ok(None)
}

#[derive(Debug)]
#[non_exhaustive]
pub enum DialogError {
    Read(io::Error),
    Write(io::Error),
    RawMode(io::Error),
    TooManyOptions,
    Bug(Bug),
}

#[derive(Debug)]
pub struct Bug(BugReason);

#[derive(Debug)]
enum BugReason {
    ShortCharacterNotFound,
}

impl From<BugReason> for DialogError {
    fn from(err: BugReason) -> Self {
        DialogError::Bug(Bug(err))
    }
}

impl fmt::Display for DialogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DialogError::Read(err) => write!(f, "read from tty: {err}"),
            DialogError::Write(err) => write!(f, "write to tty: {err}"),
            DialogError::RawMode(err) => write!(f, "switch to raw mode: {err}"),
            DialogError::TooManyOptions => write!(f, "invalid arguments: too many options"),
            DialogError::Bug(Bug(BugReason::ShortCharacterNotFound)) => {
                write!(f, "bug occurred: short character not found")
            }
        }
    }
}
