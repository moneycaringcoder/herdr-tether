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
//! call site already had, and the retry stops when that bound is reached, with
//! the interruption itself as the reported error.
//!
//! # Calls that retry through this module
//!
//! - `herdr_socket::read_bounded_line`, filling the socket buffer, bounded by
//!   the stream's own read timeout ([`Budget::Within`]).
//! - `status::drain_pipe`, reading a child's stdout and stderr, which are
//!   non-blocking descriptors ([`Budget::Immediate`]).
//! - `status::run_bounded`, polling the child with `try_wait`, bounded by the
//!   command deadline it already enforces ([`Budget::Until`]).
//!
//! # Calls that deliberately do not retry
//!
//! - `UnixStream::connect`, in both the event subscription and the request
//!   exchange. POSIX leaves an interrupted `connect` in progress, so calling it
//!   again reports `EALREADY` or `EISCONN` rather than completing it. Retrying
//!   here would be wrong rather than merely unnecessary; the subscription path
//!   already reconnects on its own, and the exchange path classifies the failure
//!   as a stopped Herdr.
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
//! - `Command::output` in the `check` host probe and its SSH probe. The pipes
//!   are drained with `read_to_end`, which ignores `Interrupted` by contract,
//!   and the child is reaped with `Child::wait`.
//! - `Command::status` for an interactive attach. The parent transfers no data
//!   and reaps with `Child::wait`; the attach deliberately has no deadline, so
//!   there is no bound for a retry to preserve either way.
//!
//! The `tmux`, discovery, lifecycle, and CLI callers of the bounded executor do
//! no blocking I/O of their own: they hand a command to `status::run_bounded`
//! and inherit its behaviour.

use std::io::{self, BufRead};
use std::time::{Duration, Instant};

/// The bound an interruption retry must not exceed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Budget {
    /// The call cannot block: the descriptor is non-blocking, or the wait is a
    /// `WNOHANG` poll. A retry costs one more syscall and no waiting, so there
    /// is no bound to preserve and no reason to read the clock.
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
/// Any other outcome, success or failure, is returned as it is. When the budget
/// is spent the interruption is returned rather than swallowed, so a caller that
/// classifies error kinds still sees what happened.
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
            return Err(interruption);
        }
    }
}

/// [`BufRead::fill_buf`] with the same retry.
///
/// This lives here rather than at the call site because the buffer `fill_buf`
/// returns borrows the reader, and that borrow cannot be carried out of a
/// closure, so the retry cannot be expressed against `retry_interrupted`
/// directly.
pub(crate) fn fill_buf<R: BufRead>(reader: &mut R, budget: Budget) -> io::Result<&[u8]> {
    let available = retry_interrupted(budget, || {
        reader.fill_buf().map(|available| available.len())
    })?;
    if available == 0 {
        // End of input: there is no buffered slice to hand back, and filling
        // again would issue a second read that could be interrupted outside the
        // retry above. A non-empty buffer is returned from the reader's own
        // buffer without touching the descriptor.
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
    fn a_bounded_wait_stays_bounded_under_repeated_interruption() {
        // Every retry restarts a relative socket timeout, so an unconditional
        // loop here would wait forever while signals keep arriving.
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Within(Duration::ZERO), || {
            attempts += 1;
            Err::<(), _>(interrupted())
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            attempts, 1,
            "an already-spent window must not buy another wait"
        );
    }

    #[test]
    fn a_deadline_bounded_wait_stops_at_the_callers_deadline() {
        let mut attempts = 0;
        let outcome = retry_interrupted(Budget::Until(Instant::now()), || {
            attempts += 1;
            Err::<(), _>(interrupted())
        });
        assert_eq!(outcome.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(attempts, 1, "a due deadline must not buy another wait");
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
