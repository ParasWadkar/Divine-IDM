//! What to do once the queue finishes: sleep, hibernate, or shut down.
//!
//! This is the most destructive thing the application can do, so the rules are
//! deliberately strict and are enforced here rather than left to callers:
//!
//! * **Never the default.** [`PostAction::Nothing`] unless explicitly chosen.
//! * **Only on success.** A failed or cancelled download aborts the action.
//!   Shutting the machine down on a failure would hide the failure and lose
//!   the chance to retry while the user was still there.
//! * **Always a countdown.** The action is armed, not taken, and anything can
//!   cancel it while it counts down. A user who wandered back to the machine
//!   gets a chance to stop it.
//! * **Cancellation is permanent.** Once cancelled, the armed action does not
//!   rearm itself from the same trigger.
//!
//! The system call sits behind [`PowerBackend`] so every rule above is
//! testable without putting the machine to sleep.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// What to do when the queue drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostAction {
    #[default]
    Nothing,
    /// Close the application, leaving the machine running.
    ExitApp,
    Sleep,
    Hibernate,
    Shutdown,
}

impl PostAction {
    /// Whether this action takes the machine away from the user.
    pub fn is_disruptive(self) -> bool {
        matches!(self, Self::Sleep | Self::Hibernate | Self::Shutdown)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Nothing => "stay running",
            Self::ExitApp => "close Divine IDM",
            Self::Sleep => "put this computer to sleep",
            Self::Hibernate => "hibernate this computer",
            Self::Shutdown => "shut this computer down",
        }
    }
}

/// Default grace period. Long enough to notice and stop, short enough to be
/// useful when leaving a machine downloading overnight.
pub const DEFAULT_COUNTDOWN: Duration = Duration::from_secs(60);

/// The minimum countdown that may be configured.
///
/// A zero-second countdown would make the action effectively uncancellable,
/// which defeats the point of arming rather than acting.
pub const MIN_COUNTDOWN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Nothing was configured.
    NotRequested,
    /// Something failed, so the action was deliberately skipped.
    SkippedOnFailure,
    /// Cancelled during the countdown.
    Cancelled,
    Performed(PostAction),
}

#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    #[error("the system refused the request: {0}")]
    Refused(String),
    #[error("this build cannot control system power state")]
    Unsupported,
}

/// The system call, behind a trait so the policy above can be tested without
/// actually suspending the machine.
pub trait PowerBackend: Send + Sync {
    fn shutdown(&self) -> Result<(), PowerError>;
    /// `hibernate` false means sleep.
    fn suspend(&self, hibernate: bool) -> Result<(), PowerError>;
}

/// Arms a post-download action and counts it down.
///
/// Cancellation is a flag rather than a channel so that any part of the
/// application — a tray menu, a keypress, a newly queued download — can stop
/// it without holding a handle.
pub struct ArmedAction {
    action: PostAction,
    countdown: Duration,
    cancelled: Arc<AtomicBool>,
}

impl ArmedAction {
    pub fn new(action: PostAction, countdown: Duration) -> Self {
        Self {
            action,
            // Clamp rather than reject: a caller asking for an impossible
            // grace period should still get a cancellable one.
            countdown: countdown.max(MIN_COUNTDOWN),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn action(&self) -> PostAction {
        self.action
    }

    pub fn countdown(&self) -> Duration {
        self.countdown
    }

    /// A handle that can cancel this action from anywhere.
    pub fn canceller(&self) -> Canceller {
        Canceller { cancelled: Arc::clone(&self.cancelled) }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Counts down and then performs the action.
    ///
    /// `all_succeeded` gates everything: a queue that ended with a failure
    /// never triggers a disruptive action, whatever was configured.
    /// `on_tick` reports remaining whole seconds so a UI can show them.
    pub async fn run<B, F>(
        self,
        backend: &B,
        all_succeeded: bool,
        mut on_tick: F,
    ) -> Result<ActionOutcome, PowerError>
    where
        B: PowerBackend + ?Sized,
        F: FnMut(Duration),
    {
        if self.action == PostAction::Nothing {
            return Ok(ActionOutcome::NotRequested);
        }
        if !all_succeeded {
            tracing::info!(
                action = ?self.action,
                "skipping the post-download action because something failed"
            );
            return Ok(ActionOutcome::SkippedOnFailure);
        }

        let step = Duration::from_millis(250);
        let mut remaining = self.countdown;
        while !remaining.is_zero() {
            if self.is_cancelled() {
                return Ok(ActionOutcome::Cancelled);
            }
            on_tick(remaining);
            let sleep_for = step.min(remaining);
            tokio::time::sleep(sleep_for).await;
            remaining = remaining.saturating_sub(sleep_for);
        }
        // Checked once more after the countdown, so a cancellation in the
        // final moment still wins.
        if self.is_cancelled() {
            return Ok(ActionOutcome::Cancelled);
        }

        match self.action {
            PostAction::Nothing => Ok(ActionOutcome::NotRequested),
            // The caller owns process lifetime; this is only the decision.
            PostAction::ExitApp => Ok(ActionOutcome::Performed(PostAction::ExitApp)),
            PostAction::Sleep => {
                backend.suspend(false)?;
                Ok(ActionOutcome::Performed(PostAction::Sleep))
            }
            PostAction::Hibernate => {
                backend.suspend(true)?;
                Ok(ActionOutcome::Performed(PostAction::Hibernate))
            }
            PostAction::Shutdown => {
                backend.shutdown()?;
                Ok(ActionOutcome::Performed(PostAction::Shutdown))
            }
        }
    }
}

/// Cancels an armed action from anywhere in the application.
#[derive(Debug, Clone)]
pub struct Canceller {
    cancelled: Arc<AtomicBool>,
}

impl Canceller {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The real thing.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPower;

impl PowerBackend for SystemPower {
    fn shutdown(&self) -> Result<(), PowerError> {
        platform::shutdown()
    }

    fn suspend(&self, hibernate: bool) -> Result<(), PowerError> {
        platform::suspend(hibernate)
    }
}

#[cfg(windows)]
mod platform {
    use super::PowerError;

    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::{
        EWX_POWEROFF, EWX_SHUTDOWN, ExitWindowsEx, SHTDN_REASON_FLAG_PLANNED,
        SHTDN_REASON_MAJOR_APPLICATION, SHTDN_REASON_MINOR_MAINTENANCE,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::{PCWSTR, w};

    /// Shutting the machine down requires `SeShutdownPrivilege`, which a
    /// process holds but does not have enabled by default.
    fn enable_shutdown_privilege() -> Result<(), PowerError> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentProcess returns a pseudo-handle needing no close;
        // `token` is a valid out-pointer.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &raw mut token,
            )
        }
        .map_err(|error| PowerError::Refused(format!("OpenProcessToken: {error}")))?;

        let mut luid = LUID::default();
        // SAFETY: a static NUL-terminated wide string and a valid out-pointer.
        let looked_up = unsafe {
            LookupPrivilegeValueW(PCWSTR::null(), w!("SeShutdownPrivilege"), &raw mut luid)
        };
        let result = looked_up.and_then(|()| {
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            // SAFETY: `token` is live and `privileges` is correctly formed.
            unsafe {
                AdjustTokenPrivileges(token, false, Some(&raw const privileges), 0, None, None)
            }
        });
        // SAFETY: `token` came from OpenProcessToken and is not used again.
        let _ = unsafe { CloseHandle(token) };
        result.map_err(|error| PowerError::Refused(format!("AdjustTokenPrivileges: {error}")))
    }

    pub(super) fn shutdown() -> Result<(), PowerError> {
        enable_shutdown_privilege()?;
        // Flagged as planned and application-initiated, so it is recorded
        // correctly in the event log rather than looking like a crash.
        // SAFETY: both arguments are plain flag values.
        unsafe {
            ExitWindowsEx(
                EWX_SHUTDOWN | EWX_POWEROFF,
                SHTDN_REASON_MAJOR_APPLICATION
                    | SHTDN_REASON_MINOR_MAINTENANCE
                    | SHTDN_REASON_FLAG_PLANNED,
            )
        }
        .map_err(|error| PowerError::Refused(format!("ExitWindowsEx: {error}")))
    }

    pub(super) fn suspend(hibernate: bool) -> Result<(), PowerError> {
        // `force` is false so applications get a chance to object, and
        // wake events are left enabled.
        // SAFETY: plain boolean arguments.
        let ok = unsafe { SetSuspendState(hibernate, false, false) };
        if ok { Ok(()) } else { Err(PowerError::Refused("SetSuspendState returned false".into())) }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::PowerError;

    pub(super) fn shutdown() -> Result<(), PowerError> {
        Err(PowerError::Unsupported)
    }

    pub(super) fn suspend(_hibernate: bool) -> Result<(), PowerError> {
        Err(PowerError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to do instead of doing it.
    #[derive(Default)]
    struct FakePower {
        calls: Mutex<Vec<String>>,
    }

    impl FakePower {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PowerBackend for FakePower {
        fn shutdown(&self) -> Result<(), PowerError> {
            self.calls.lock().unwrap().push("shutdown".into());
            Ok(())
        }

        fn suspend(&self, hibernate: bool) -> Result<(), PowerError> {
            self.calls.lock().unwrap().push(if hibernate {
                "hibernate".into()
            } else {
                "sleep".into()
            });
            Ok(())
        }
    }

    /// A countdown short enough for a test but still clamped to the minimum.
    fn armed(action: PostAction) -> ArmedAction {
        ArmedAction::new(action, Duration::from_millis(1))
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_happens_unless_an_action_was_chosen() {
        let power = FakePower::default();
        let outcome = armed(PostAction::Nothing).run(&power, true, |_| {}).await.unwrap();
        assert_eq!(outcome, ActionOutcome::NotRequested);
        assert!(power.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_download_never_shuts_the_machine_down() {
        // The important one. Shutting down on failure hides the failure and
        // takes away the chance to retry while the user is still there.
        let power = FakePower::default();
        let outcome = armed(PostAction::Shutdown).run(&power, false, |_| {}).await.unwrap();
        assert_eq!(outcome, ActionOutcome::SkippedOnFailure);
        assert!(power.calls().is_empty(), "nothing should have been attempted");
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_queue_performs_the_action() {
        let power = FakePower::default();
        let outcome = armed(PostAction::Shutdown).run(&power, true, |_| {}).await.unwrap();
        assert_eq!(outcome, ActionOutcome::Performed(PostAction::Shutdown));
        assert_eq!(power.calls(), vec!["shutdown"]);
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_and_hibernate_are_distinct() {
        let power = FakePower::default();
        armed(PostAction::Sleep).run(&power, true, |_| {}).await.unwrap();
        armed(PostAction::Hibernate).run(&power, true, |_| {}).await.unwrap();
        assert_eq!(power.calls(), vec!["sleep", "hibernate"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_during_the_countdown_stops_it() {
        let power = FakePower::default();
        let action = ArmedAction::new(PostAction::Shutdown, Duration::from_secs(60));
        let canceller = action.canceller();

        // Cancel the moment the first tick is reported.
        let outcome = action.run(&power, true, |_| canceller.cancel()).await.unwrap();

        assert_eq!(outcome, ActionOutcome::Cancelled);
        assert!(power.calls().is_empty(), "a cancelled action must not run");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_before_the_countdown_starts_also_works() {
        let power = FakePower::default();
        let action = ArmedAction::new(PostAction::Shutdown, Duration::from_secs(60));
        action.canceller().cancel();
        let outcome = action.run(&power, true, |_| {}).await.unwrap();
        assert_eq!(outcome, ActionOutcome::Cancelled);
        assert!(power.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_countdown_is_reported_so_a_ui_can_show_it() {
        let power = FakePower::default();
        let ticks = std::sync::Mutex::new(Vec::new());
        ArmedAction::new(PostAction::Sleep, Duration::from_secs(10))
            .run(&power, true, |remaining| ticks.lock().unwrap().push(remaining))
            .await
            .unwrap();

        let ticks = ticks.into_inner().unwrap();
        assert!(ticks.len() > 1, "should report progress, got {ticks:?}");
        // Counts down, never up.
        for pair in ticks.windows(2) {
            assert!(pair[0] >= pair[1], "countdown went backwards: {pair:?}");
        }
    }

    #[test]
    fn a_countdown_shorter_than_the_minimum_is_clamped() {
        // A zero-second countdown would be uncancellable, which defeats the
        // point of arming rather than acting.
        let action = ArmedAction::new(PostAction::Shutdown, Duration::ZERO);
        assert_eq!(action.countdown(), MIN_COUNTDOWN);
        assert!(!action.is_cancelled());
    }

    #[test]
    fn disruptive_actions_are_identified() {
        assert!(PostAction::Shutdown.is_disruptive());
        assert!(PostAction::Sleep.is_disruptive());
        assert!(PostAction::Hibernate.is_disruptive());
        // These leave the machine usable.
        assert!(!PostAction::Nothing.is_disruptive());
        assert!(!PostAction::ExitApp.is_disruptive());
    }

    #[test]
    fn the_default_is_to_do_nothing() {
        assert_eq!(PostAction::default(), PostAction::Nothing);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusing_backend_surfaces_the_error() {
        struct Refusing;
        impl PowerBackend for Refusing {
            fn shutdown(&self) -> Result<(), PowerError> {
                Err(PowerError::Refused("no privilege".into()))
            }
            fn suspend(&self, _: bool) -> Result<(), PowerError> {
                Err(PowerError::Refused("no privilege".into()))
            }
        }
        let outcome = armed(PostAction::Shutdown).run(&Refusing, true, |_| {}).await;
        assert!(outcome.is_err(), "a refusal should be reported, not swallowed");
    }
}
