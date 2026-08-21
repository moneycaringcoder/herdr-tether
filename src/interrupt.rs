//! One definition of what Tether does when a blocking call is interrupted by a
//! signal.
//!
//! A signal delivered while a thread is blocked in a read, a write, or a wait
//! surfaces as `EINTR`. It reports no failure the caller can act on: the socket
//! is still connected, the child is still running, and the call simply has to be
//! made again. Herdr's own TUI raises `SIGWINCH` freely, so any blocking call on
//! the socket, `tmux`, or SSH paths can span one.
//!
//! Every retry on those paths goes through [`retry_interrupted`], so the
//! behaviour is defined here and nowhere else. On those paths a bare `?` on a
//! blocking read, write, or wait is a bug unless it is one of the calls listed
//! under "Calls that deliberately do not retry" below, which are the ones where
//! retrying is either wrong or already done for us.
//!
//! # A retry must not unbound a bounded wait
//!
//! The waits on these paths are bounded: the Herdr socket sets `SO_RCVTIMEO` and
//! `SO_SNDTIMEO`, and the bounded process executor enforces an absolute
//! deadline. A relative socket timeout restarts on every call, so a loop that
//! retries an interrupted read without limit turns a bounded wait into an
//! unbounded one when signals keep arriving. [`Budget`] carries the bound the
//! call site already had. For a relative timeout the retries continue for at
//! most one further window, measured from the first interruption, so the worst
//! case is the call's own window plus one more rather than one fresh window per
//! signal. A spent bound is reported as `ErrorKind::TimedOut`, because that is
//! what the wait elapsing means to the caller, with the interruption kept as the
//! error's source.
//!
//! # Calls that retry through this module
//!
//! - `herdr_socket::read_bounded_line`, filling the socket buffer, bounded by
//!   the stream's own read timeout ([`Budget::Within`]).
//! - `status::drain_pipe`, reading a child's stdout and stderr, which are
//!   non-blocking descriptors ([`Budget::Immediate`]).
//! - `status::run_bounded`, polling the child with `try_wait`, bounded by the
//!   command deadline it already enforces ([`Budget::Until`]).
//! - `orchestration::read_bounded_prompt_from`, reading the reviewed Mission
//!   Control prompt from the terminal a byte at a time ([`Budget::Immediate`]).
//!
//! # Calls that deliberately do not retry
//!
//! - `UnixStream::connect`, in both the event subscription and the request
//!   exchange. After an interrupted `connect` the connection continues to be
//!   established asynchronously and the socket's state is unspecified, so the
//!   documented recovery is a fresh socket rather than a second `connect` on the
//!   same one. Both call sites already do that: the subscription supervisor
//!   reconnects, and the exchange classifies the failure as a stopped Herdr.
//! - `write_all` on the socket. The standard library's `Write::write_all`
//!   ignores `Interrupted` and resumes from the unwritten remainder, and
//!   `UnixStream` does not override it.
//! - `flush` on the socket. Flushing a `UnixStream` returns `Ok(())` without a
//!   syscall; there is nothing to interrupt.
//! - `shutdown(Shutdown::Write)` on the socket. Half-closing transfers no data
//!   and does not block.
//! - `Command::spawn` for a bounded `tmux` or SSH command. The fork and exec are
//!   not a blocking transfer, and the standard library's Unix implementation
//!   already retries the interrupted parent-side read of the exec status pipe.
//! - `Child::wait` after killing a timed-out or cancelled child. The standard
//!   library repeats an interrupted `waitpid` here, so the child is still
//!   reaped. `Child::try_wait` does not repeat it, which is why the poll in
//!   `status::run_bounded` goes through this module.
//! - `Command::output`, which no longer has a production caller: the host
//!   `check` probes that used it now run through the bounded executor, and so
//!   retry through this module.
//! - `Command::status`, for an interactive attach and for the `herdr server
//!   reload-config` call after a keybinding change. The parent transfers no data
//!   and reaps with `Child::wait`; neither has a deadline, so there is no bound
//!   for a retry to preserve either way.
//! - `read_line` on stdin: the `SEND` confirmation beside the reviewed prompt,
//!   and the two interactive confirmations in `cli`. `BufRead::read_line` is
//!   defined in terms of `read_until`, which ignores `Interrupted` and resumes,
//!   so the line survives without help from here.
//! - `crossterm::event::{poll, read}`, in Mission Control, the Observer manager,
//!   and the picker. Crossterm retries `EINTR` inside its own poll loop, so an
//!   interrupted wait for a key returns no event rather than an error.
//!
//! The `tmux`, discovery, lifecycle, and CLI callers of `status::run_bounded` do
//! no blocking I/O of their own: they hand it a command and inherit its
//! behaviour.
//!
//! Terminal input is on these paths too, on narrower grounds. Nothing in the
//! process is known to interrupt a terminal read today: reads happen in canonical
//! mode, and the `SIGWINCH` handler crossterm installs comes through signal-hook,
//! which sets `SA_RESTART`, so the kernel restarts the read rather than failing
//! it. That is a property of the handlers currently installed, not a guarantee
//! about the call, and a handler added later - here or in a dependency - only has
//! to omit the flag. So a blocking terminal read whose loss cannot be recovered
//! goes through this module as well, and the terminal reads that are already safe
//! by contract are listed above rather than left for a reader to re-derive.

use std::io::{self, BufRead, BufReader, Read};
use std::time::{Duration, Instant};

/// The bound an interruption retry must not exceed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Budget {
    /// There is no bound for a retry to preserve, so nothing reads the clock.
    ///
    /// Two kinds of call qualify. One cannot block at all - a non-blocking
    /// descriptor, or a `WNOHANG` poll - so a retry costs one more syscall and no
    /// waiting. The other blocks with no deadline by design: terminal input waits
    /// for a person to type, and no retry can make that wait longer than it
    /// already is.
    Immediate,
    /// The call blocks under a relative timeout, such as a socket's
    /// `SO_RCVTIMEO`, which every retry restarts. Retries are allowed for one
    /// further window measured from the first interruption, rather than one
    /// fresh window per signal.
    Within(Duration),
    /// The call sits inside a loop the caller has already bounded with an
    /// absolute deadline. Retries stop at that same deadline.
    Until(Instant),
}

/// Repeats `operation` while it reports `ErrorKind::Interrupted`, within
/// `budget`.
///
/// Any other outcome, success or failure, is returned as it is. A spent budget
/// is reported as `ErrorKind::TimedOut` with the interruption as its source: the
/// wait the call site asked for has elapsed, which is what its callers already
/// classify, and reporting `Interrupted` there would make a bounded wait look
/// like a connection failure to code that has no reason to treat it as one.
pub(crate) fn retry_interrupted<T>(
    budget: Budget,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let mut window_deadline = None;
    loop {
        let interruption = match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => error,
            outcome => return outcome,
        };
        let deadline = match budget {
            Budget::Immediate => continue,
            // The clock is read only once a signal has actually arrived, so an
            // uninterrupted call pays nothing for the bound.
            Budget::Within(window) => {
                *window_deadline.get_or_insert_with(|| Instant::now() + window)
            }
            Budget::Until(deadline) => deadline,
        };
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, interruption));
        }
    }
}

/// [`BufRead::fill_buf`] with the same retry.
///
/// This lives here rather than at the call site because the buffer `fill_buf`
/// returns borrows the reader, and that borrow cannot be carried out of a
/// closure, so the retry cannot be expressed against `retry_interrupted`
/// directly. It takes a [`BufReader`] rather than any [`BufRead`] because the
/// second call below relies on the buffered contents being handed back without
/// a further read, which `BufReader` guarantees and the trait does not.
pub(crate) fn fill_buf<R: Read>(reader: &mut BufReader<R>, budget: Budget) -> io::Result<&[u8]> {
    let available = retry_interrupted(budget, || {
        reader.fill_buf().map(|available| available.len())
    })?;
    if available == 0 {
        // End of input: there is no buffered slice to hand back, and filling
        // again would issue a second read that could be interrupted outside the
        // retry above.
        return Ok(&[]);
    }
    reader.fill_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interrupted() -> io::Error {
        io::Error::from(io::ErrorKind::Interrupted)
    }

    #[test]
    fn a_non_blocking_call_is_retried_until_it_reports_something_else() {
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Immediate, || {
            attempts += 1;
            if attempts < 4 {
                return Err(interrupted());
            }
            Ok(attempts)
        });
        assert_eq!(outcome.unwrap(), 4);
    }

    #[test]
    fn an_error_that_is_not_an_interruption_is_reported_as_it_is() {
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Within(Duration::from_secs(30)), || {
            attempts += 1;
            Err::<(), _>(io::Error::from(io::ErrorKind::ConnectionReset))
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(attempts, 1, "a reported failure must not be retried");
    }

    #[test]
    fn an_already_spent_window_does_not_buy_another_wait() {
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Within(Duration::ZERO), || {
            attempts += 1;
            Err::<(), _>(interrupted())
        });
        let error = outcome.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .map(io::Error::kind),
            Some(io::ErrorKind::Interrupted),
            "the interruption must survive as the source"
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn a_bounded_wait_ends_instead_of_restarting_its_window_per_signal() {
        // This is the property the module exists for: a window granted per
        // signal never closes while signals keep arriving, so the call would
        // never return. The window is granted once, from the first
        // interruption, and a short one is enough to prove it terminates.
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Within(Duration::from_millis(20)), || {
            attempts += 1;
            Err::<(), _>(interrupted())
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(
            attempts > 1,
            "the window must allow retries before it is spent"
        );
    }

    #[test]
    fn a_deadline_bounded_wait_stops_at_the_callers_deadline() {
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Until(Instant::now()), || {
            attempts += 1;
            Err::<(), _>(interrupted())
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(attempts, 1, "a due deadline must not buy another wait");
    }

    #[test]
    fn an_interrupted_poll_before_the_deadline_is_retried() {
        // The shape of the `try_wait` poll in the bounded process executor: an
        // interruption while the deadline is still live must not end the
        // command, and the poll's own answer must be what the caller sees.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Until(deadline), || {
            attempts += 1;
            if attempts == 1 {
                return Err(interrupted());
            }
            Ok::<Option<u8>, io::Error>(None)
        });
        assert_eq!(outcome.unwrap(), None);
        assert_eq!(attempts, 2);
    }

    /// A reader that reports an interruption before each byte it yields.
    struct SignalInterrupted {
        remaining: &'static [u8],
        interrupted: bool,
    }

    impl io::Read for SignalInterrupted {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(interrupted());
            }
            self.interrupted = false;
            if buffer.is_empty() || self.remaining.is_empty() {
                return Ok(0);
            }
            buffer[0] = self.remaining[0];
            self.remaining = &self.remaining[1..];
            Ok(1)
        }
    }

    #[test]
    fn filling_a_buffer_retries_an_interrupted_read() {
        let mut reader = io::BufReader::new(SignalInterrupted {
            remaining: b"ok",
            interrupted: false,
        });
        assert_eq!(
            fill_buf(&mut reader, Budget::Within(Duration::from_secs(30))).unwrap(),
            b"o"
        );
    }

    #[test]
    fn filling_a_buffer_reports_end_of_input_as_an_empty_slice() {
        let mut reader = io::BufReader::new(SignalInterrupted {
            remaining: b"",
            interrupted: false,
        });
        assert!(
            fill_buf(&mut reader, Budget::Within(Duration::from_secs(30)))
                .unwrap()
                .is_empty()
        );
    }
}
