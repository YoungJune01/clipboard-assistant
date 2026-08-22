use std::{
    collections::VecDeque,
    sync::{Arc, Barrier, Mutex},
};

use crate::{
    domain::{ClipboardRepresentation, PasteFallbackReason, PasteOutcome},
    services::paste::{
        PasteClipboard, PasteInput, PastePanel, PasteTarget, QuickPanel,
        QuickPanelPasteCoordinator, SafePasteService, TargetIdentity, TargetSnapshot,
    },
};

#[test]
fn safe_path_publishes_hides_revalidates_and_only_then_sends_paste() {
    let fixture = Fixture::safe();

    fixture.service.prepare_target().unwrap();
    let outcome = fixture.service.paste(&text("selected secret")).unwrap();

    assert_eq!(outcome, PasteOutcome::CommandSent);
    assert_eq!(outcome.user_message(), "Paste command sent");
    assert_eq!(
        fixture.calls(),
        [
            Call::Capture,
            Call::Inspect,
            Call::InputAllowed,
            Call::InputDesktop,
            Call::Publish,
            Call::Hide,
            Call::Restore,
            Call::Inspect,
            Call::InputAllowed,
            Call::InputDesktop,
            Call::Foreground,
            Call::Inject,
        ]
    );
}

#[test]
fn changed_window_identity_never_injects() {
    let baseline = identity();
    let variants = [
        TargetIdentity {
            window_instance_id: 1001,
            ..baseline
        },
        TargetIdentity {
            process_id: 88,
            ..baseline
        },
        TargetIdentity {
            thread_id: 99,
            ..baseline
        },
        TargetIdentity {
            process_started_at: 1000,
            ..baseline
        },
        TargetIdentity {
            session_id: 4,
            ..baseline
        },
        TargetIdentity {
            desktop_id: 9,
            ..baseline
        },
        TargetIdentity {
            integrity_level: 0x3000,
            ..baseline
        },
        TargetIdentity {
            restricted: true,
            ..baseline
        },
        TargetIdentity {
            app_container: true,
            ..baseline
        },
    ];

    for changed in variants {
        let fixture = Fixture::with_inspections([Ok(changed)]);
        fixture.service.prepare_target().unwrap();

        assert_copy_only(fixture.service.paste(&text("value")).unwrap());
        assert!(!fixture.calls().contains(&Call::Inject));
    }
}

#[test]
fn missing_target_and_any_identity_query_failure_are_copy_only() {
    for inspection in [
        Err(FakeError("target disappeared")),
        Err(FakeError("query failed")),
    ] {
        let fixture = Fixture::with_inspections([inspection]);
        fixture.service.prepare_target().unwrap();

        assert_copy_only(fixture.service.paste(&text("value")).unwrap());
        assert!(!fixture.calls().contains(&Call::Restore));
        assert!(!fixture.calls().contains(&Call::Inject));
    }
}

#[test]
fn foreground_restore_failure_and_unrelated_foreground_are_copy_only() {
    let restore_failure = Fixture::safe();
    restore_failure
        .target
        .restore_results
        .lock()
        .unwrap()
        .push_back(Err(FakeError("denied")));
    restore_failure.service.prepare_target().unwrap();
    assert_copy_only(restore_failure.service.paste(&text("value")).unwrap());
    assert!(!restore_failure.calls().contains(&Call::Inject));

    let unrelated = Fixture::safe();
    *unrelated.target.foreground.lock().unwrap() = Ok(Window(777));
    unrelated.service.prepare_target().unwrap();
    assert_copy_only(unrelated.service.paste(&text("value")).unwrap());
    assert!(!unrelated.calls().contains(&Call::Inject));
}

#[test]
fn elevated_restricted_different_desktop_or_session_targets_are_copy_only() {
    let cases = [
        TargetIdentity {
            integrity_level: 0x3000,
            ..identity()
        },
        TargetIdentity {
            restricted: true,
            ..identity()
        },
        TargetIdentity {
            app_container: true,
            ..identity()
        },
        TargetIdentity {
            desktop_id: 12,
            ..identity()
        },
        TargetIdentity {
            session_id: 8,
            ..identity()
        },
    ];

    for target in cases {
        let fixture = Fixture::with_target(target);
        fixture.service.prepare_target().unwrap();
        assert_copy_only(fixture.service.paste(&text("value")).unwrap());
        assert!(!fixture.calls().contains(&Call::Inject));
    }
}

#[test]
fn failures_after_clipboard_publish_leave_content_copied_and_do_not_inject() {
    let fixture = Fixture::safe();
    fixture
        .target
        .input_allowed
        .lock()
        .unwrap()
        .push_back(Err(FakeError("token query")));
    fixture.service.prepare_target().unwrap();

    let outcome = fixture.service.paste(&text("copied payload")).unwrap();

    assert_copy_only(outcome);
    assert_eq!(fixture.clipboard.published.lock().unwrap().len(), 1);
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn desktop_and_foreground_api_failures_are_copy_only() {
    let desktop = Fixture::safe();
    *desktop.target.input_desktop.lock().unwrap() = Err(FakeError("desktop query"));
    desktop.service.prepare_target().unwrap();
    assert_copy_only(desktop.service.paste(&text("payload")).unwrap());
    assert!(!desktop.calls().contains(&Call::Inject));

    let foreground = Fixture::safe();
    *foreground.target.foreground.lock().unwrap() = Err(FakeError("foreground query"));
    foreground.service.prepare_target().unwrap();
    assert_copy_only(foreground.service.paste(&text("payload")).unwrap());
    assert!(!foreground.calls().contains(&Call::Inject));
}

#[test]
fn panel_hide_failure_is_conservative_even_when_target_is_otherwise_safe() {
    let fixture = Fixture::safe();
    *fixture.panel.result.lock().unwrap() = Err(FakeError("hide failed"));
    fixture.service.prepare_target().unwrap();

    assert!(fixture.service.paste(&text("copied payload")).is_err());
    assert_eq!(fixture.clipboard.published.lock().unwrap().len(), 1);
    assert!(!fixture.calls().contains(&Call::Restore));
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn panel_reported_visible_after_hide_is_an_infrastructure_error() {
    let fixture = Fixture::safe();
    *fixture.panel.visible.lock().unwrap() = Ok(true);
    fixture
        .panel
        .keep_visible_after_hide
        .store(true, std::sync::atomic::Ordering::Relaxed);
    fixture.service.prepare_target().unwrap();

    assert!(fixture.service.paste(&text("copied payload")).is_err());
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn every_successful_outcome_leaves_the_panel_hidden() {
    let no_target = Fixture::safe();
    let fixtures = [
        Fixture::safe(),
        Fixture::with_inspections([Err(FakeError("gone"))]),
        no_target,
    ];
    for (index, fixture) in fixtures.into_iter().enumerate() {
        if index != 2 {
            fixture.service.prepare_target().unwrap();
        }
        let outcome = fixture.service.paste(&text("payload"));

        assert!(outcome.is_ok());
        assert_eq!(*fixture.panel.visible.lock().unwrap(), Ok(false));
    }
}

#[test]
fn failed_or_partial_input_dispatch_is_copy_only_without_retrying() {
    let fixture = Fixture::safe();
    *fixture.input.result.lock().unwrap() = Err(FakeError("partial send"));
    fixture.service.prepare_target().unwrap();

    assert_copy_only(fixture.service.paste(&text("copied payload")).unwrap());
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|call| **call == Call::Inject)
            .count(),
        1
    );
}

#[test]
fn target_capture_failure_still_allows_copy_only_and_clears_an_old_token() {
    let fixture = Fixture::safe();
    fixture.service.prepare_target().unwrap();
    fixture
        .target
        .captures
        .lock()
        .unwrap()
        .push_back(Err(FakeError("no foreground target")));
    assert!(fixture.service.prepare_target().is_err());

    assert_copy_only(fixture.service.paste(&text("copied payload")).unwrap());
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn coordinator_opens_panel_when_target_capture_fails_and_later_copies_only() {
    let fixture = Fixture::safe();
    *fixture.target.captures.lock().unwrap() =
        VecDeque::from([Err(FakeError("no foreground target"))]);
    let quick_panel = FakeQuickPanel::default();
    let coordinator = QuickPanelPasteCoordinator::new(quick_panel.clone(), fixture.service);

    coordinator.show().unwrap();
    let outcome = coordinator.paste(&text("copied payload")).unwrap();

    assert_eq!(quick_panel.show_count(), 1);
    assert_copy_only(outcome);
    assert!(!fixture.calls.lock().unwrap().contains(&Call::Inject));
}

#[test]
fn coordinator_does_not_replace_target_while_panel_is_already_visible() {
    let fixture = Fixture::safe();
    let quick_panel = FakeQuickPanel::default();
    let coordinator = QuickPanelPasteCoordinator::new(quick_panel.clone(), fixture.service);

    coordinator.show().unwrap();
    coordinator.show().unwrap();

    assert_eq!(quick_panel.show_count(), 1);
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == Call::Capture)
            .count(),
        1
    );
}

#[test]
fn clipboard_publish_failure_consumes_the_target_and_never_hides_or_injects() {
    let fixture = Fixture::safe();
    *fixture.clipboard.result.lock().unwrap() = Err(FakeError("clipboard busy"));
    fixture.service.prepare_target().unwrap();

    assert!(fixture.service.paste(&text("payload")).is_err());
    assert!(!fixture.calls().contains(&Call::Hide));
    assert!(!fixture.calls().contains(&Call::Inject));

    *fixture.clipboard.result.lock().unwrap() = Ok(55);
    assert_copy_only(fixture.service.paste(&text("payload")).unwrap());
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn one_prepared_target_can_dispatch_at_most_one_paste_command() {
    let fixture = Fixture::safe();
    fixture.service.prepare_target().unwrap();

    assert_eq!(
        fixture.service.paste(&text("first")).unwrap(),
        PasteOutcome::CommandSent
    );
    assert_copy_only(fixture.service.paste(&text("second")).unwrap());
    assert_eq!(
        fixture
            .calls()
            .iter()
            .filter(|call| **call == Call::Inject)
            .count(),
        1
    );
}

#[test]
fn explicitly_cleared_target_cannot_inject() {
    let fixture = Fixture::safe();
    fixture.service.prepare_target().unwrap();
    fixture.service.clear_target();

    assert_copy_only(fixture.service.paste(&text("payload")).unwrap());
    assert!(!fixture.calls().contains(&Call::Inject));
}

#[test]
fn concurrent_paste_requests_are_serialized_and_inject_at_most_once() {
    let fixture = Fixture::safe();
    fixture.service.prepare_target().unwrap();
    let calls = Arc::clone(&fixture.calls);
    let service = Arc::new(fixture.service);
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();

    for label in ["first", "second"] {
        let service = Arc::clone(&service);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            service.paste(&text(label)).unwrap()
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == PasteOutcome::CommandSent)
            .count(),
        1
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == Call::Inject)
            .count(),
        1
    );
}

fn assert_copy_only(outcome: PasteOutcome) {
    assert_eq!(
        outcome,
        PasteOutcome::CopyOnly {
            reason: PasteFallbackReason::UnsafeTarget,
        }
    );
    assert_eq!(
        outcome.user_message(),
        "Cannot paste safely; content was copied. Paste it manually."
    );
}

fn text(value: &str) -> Vec<ClipboardRepresentation> {
    vec![ClipboardRepresentation::UnicodeText {
        text: value.to_owned(),
    }]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window(usize);

fn identity() -> TargetIdentity {
    TargetIdentity {
        window_instance_id: 1000,
        process_id: 42,
        thread_id: 43,
        process_started_at: 999,
        session_id: 3,
        desktop_id: 7,
        integrity_level: 0x2000,
        restricted: false,
        app_container: false,
    }
}

type Service = SafePasteService<FakeTarget, FakeClipboard, FakePanel, FakeInput>;

struct Fixture {
    service: Service,
    target: FakeTarget,
    clipboard: FakeClipboard,
    panel: FakePanel,
    input: FakeInput,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Fixture {
    fn safe() -> Self {
        Self::with_target(identity())
    }

    fn with_target(target_identity: TargetIdentity) -> Self {
        Self::with_inspections([Ok(target_identity), Ok(target_identity)])
            .with_capture_identity(target_identity)
    }

    fn with_inspections(
        inspections: impl IntoIterator<Item = Result<TargetIdentity, FakeError>>,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let target = FakeTarget {
            calls: Arc::clone(&calls),
            captures: Arc::new(Mutex::new(VecDeque::from([Ok(TargetSnapshot {
                window: Window(100),
                focused_control: Some(Window(101)),
                identity: identity(),
                window_generation: 1,
                focused_control_instance_id: Some(2),
                focused_control_generation: Some(3),
            })]))),
            inspections: Arc::new(Mutex::new(inspections.into_iter().collect())),
            restore_results: Arc::new(Mutex::new(VecDeque::new())),
            input_allowed: Arc::new(Mutex::new(VecDeque::new())),
            input_desktop: Arc::new(Mutex::new(Ok(7))),
            foreground: Arc::new(Mutex::new(Ok(Window(100)))),
            lifecycle_valid: Arc::new(Mutex::new(VecDeque::new())),
        };
        let clipboard = FakeClipboard {
            calls: Arc::clone(&calls),
            published: Arc::new(Mutex::new(Vec::new())),
            result: Arc::new(Mutex::new(Ok(55))),
        };
        let panel = FakePanel {
            calls: Arc::clone(&calls),
            result: Arc::new(Mutex::new(Ok(()))),
            visible: Arc::new(Mutex::new(Ok(true))),
            keep_visible_after_hide: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let input = FakeInput {
            calls: Arc::clone(&calls),
            result: Arc::new(Mutex::new(Ok(()))),
        };
        let service = SafePasteService::new(
            target.clone(),
            clipboard.clone(),
            panel.clone(),
            input.clone(),
        );
        Self {
            service,
            target,
            clipboard,
            panel,
            input,
            calls,
        }
    }

    fn with_capture_identity(self, identity: TargetIdentity) -> Self {
        *self.target.captures.lock().unwrap() = VecDeque::from([Ok(TargetSnapshot {
            window: Window(100),
            focused_control: Some(Window(101)),
            identity,
            window_generation: 1,
            focused_control_instance_id: Some(2),
            focused_control_generation: Some(3),
        })]);
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct FakeTarget {
    calls: Arc<Mutex<Vec<Call>>>,
    captures: Arc<Mutex<CaptureResults>>,
    inspections: Arc<Mutex<VecDeque<Result<TargetIdentity, FakeError>>>>,
    restore_results: Arc<Mutex<VecDeque<Result<(), FakeError>>>>,
    input_allowed: Arc<Mutex<VecDeque<Result<bool, FakeError>>>>,
    input_desktop: Arc<Mutex<Result<u64, FakeError>>>,
    foreground: Arc<Mutex<Result<Window, FakeError>>>,
    lifecycle_valid: Arc<Mutex<VecDeque<Result<bool, FakeError>>>>,
}

type CaptureResults = VecDeque<Result<TargetSnapshot<Window>, FakeError>>;

impl PasteTarget for FakeTarget {
    type Error = FakeError;
    type Window = Window;

    fn capture(&self) -> Result<TargetSnapshot<Self::Window>, Self::Error> {
        self.calls.lock().unwrap().push(Call::Capture);
        self.captures
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(FakeError("missing capture")))
    }

    fn inspect(&self, _window: Self::Window) -> Result<TargetIdentity, Self::Error> {
        self.calls.lock().unwrap().push(Call::Inspect);
        self.inspections
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(identity()))
    }

    fn input_allowed(&self, identity: &TargetIdentity) -> Result<bool, Self::Error> {
        self.calls.lock().unwrap().push(Call::InputAllowed);
        self.input_allowed
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(identity.session_id == 3
                && identity.integrity_level <= 0x2000
                && !identity.restricted
                && !identity.app_container))
    }

    fn input_desktop(&self) -> Result<u64, Self::Error> {
        self.calls.lock().unwrap().push(Call::InputDesktop);
        *self.input_desktop.lock().unwrap()
    }

    fn restore(&self, _target: &TargetSnapshot<Self::Window>) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Restore);
        self.restore_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn foreground(&self) -> Result<Self::Window, Self::Error> {
        self.calls.lock().unwrap().push(Call::Foreground);
        *self.foreground.lock().unwrap()
    }

    fn lifecycle_valid(&self, _target: &TargetSnapshot<Self::Window>) -> Result<bool, Self::Error> {
        self.lifecycle_valid
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(true))
    }
}

#[derive(Clone)]
struct FakeClipboard {
    calls: Arc<Mutex<Vec<Call>>>,
    published: Arc<Mutex<Vec<Vec<ClipboardRepresentation>>>>,
    result: Arc<Mutex<Result<u32, FakeError>>>,
}

impl PasteClipboard for FakeClipboard {
    type Error = FakeError;

    fn publish(&self, representations: &[ClipboardRepresentation]) -> Result<u32, Self::Error> {
        self.calls.lock().unwrap().push(Call::Publish);
        self.published
            .lock()
            .unwrap()
            .push(representations.to_vec());
        *self.result.lock().unwrap()
    }
}

#[derive(Clone)]
struct FakePanel {
    calls: Arc<Mutex<Vec<Call>>>,
    result: Arc<Mutex<Result<(), FakeError>>>,
    visible: Arc<Mutex<Result<bool, FakeError>>>,
    keep_visible_after_hide: Arc<std::sync::atomic::AtomicBool>,
}

impl PastePanel for FakePanel {
    type Error = FakeError;

    fn hide(&self) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Hide);
        let result = *self.result.lock().unwrap();
        if result.is_ok()
            && !self
                .keep_visible_after_hide
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            *self.visible.lock().unwrap() = Ok(false);
        }
        result
    }

    fn is_visible(&self) -> Result<bool, Self::Error> {
        *self.visible.lock().unwrap()
    }
}

#[derive(Clone, Default)]
struct FakeQuickPanel {
    shown: Arc<Mutex<usize>>,
    visible: Arc<Mutex<bool>>,
}

impl FakeQuickPanel {
    fn show_count(&self) -> usize {
        *self.shown.lock().unwrap()
    }
}

impl QuickPanel for FakeQuickPanel {
    type Error = FakeError;

    fn show(&self) -> Result<(), Self::Error> {
        *self.shown.lock().unwrap() += 1;
        *self.visible.lock().unwrap() = true;
        Ok(())
    }

    fn hide(&self) -> Result<(), Self::Error> {
        *self.visible.lock().unwrap() = false;
        Ok(())
    }

    fn is_visible(&self) -> bool {
        *self.visible.lock().unwrap()
    }
}

#[derive(Clone)]
struct FakeInput {
    calls: Arc<Mutex<Vec<Call>>>,
    result: Arc<Mutex<Result<(), FakeError>>>,
}

impl PasteInput<Window> for FakeInput {
    type Error = FakeError;

    fn send_ctrl_v(&self, _target: &TargetSnapshot<Window>) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Inject);
        *self.result.lock().unwrap()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Capture,
    Publish,
    Hide,
    Inspect,
    InputAllowed,
    InputDesktop,
    Restore,
    Foreground,
    Inject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FakeError(&'static str);

impl std::fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FakeError {}
