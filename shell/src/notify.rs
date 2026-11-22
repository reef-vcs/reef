//! Banners through `UNUserNotificationCenter` (R3.39). Tauri's notification
//! plugin posts through the deprecated `NSUserNotification` on macOS and
//! reports permission as always granted, so the app cannot say when
//! notifications are off or route a click.

use std::ptr::NonNull;
use std::sync::OnceLock;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, define_class, msg_send};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use reef_client::web::WebPath;
use reef_shell::notice::Note;
use reef_shell::{Command, ShellHandle};

static SHELL: OnceLock<ShellHandle> = OnceLock::new();

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ReefNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        // macOS presents nothing while the app is frontmost unless this
        // returns presentation options (UNUserNotificationCenter.h).
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &block2::DynBlock<dyn Fn()>,
        ) {
            let target = response
                .notification()
                .request()
                .content()
                .targetContentIdentifier()
                .map(|target| target.to_string());
            let path = target
                .as_deref()
                .and_then(WebPath::parse)
                .unwrap_or_else(WebPath::start);
            if let Some(shell) = SHELL.get() {
                shell.send(Command::Open(path));
            }
            done.call(());
        }
    }
);

impl Delegate {
    fn new() -> Retained<Delegate> {
        let this = Delegate::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

pub fn init(shell: ShellHandle) {
    let _ = SHELL.set(shell);
    let center = UNUserNotificationCenter::currentNotificationCenter();
    // The center keeps a weak reference: the delegate lives as long as the app.
    let delegate = Delegate::new();
    center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    std::mem::forget(delegate);
    std::thread::spawn(|| {
        let answered = RcBlock::new(|granted: Bool, error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                tracing::warn!(error = %error.localizedDescription(), "notification permission");
            }
            tracing::debug!(granted = granted.as_bool(), "notification permission");
            if let Some(shell) = SHELL.get() {
                shell.send(Command::Notifications(granted.as_bool()));
            }
        });
        UNUserNotificationCenter::currentNotificationCenter()
            .requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert
                    | UNAuthorizationOptions::Sound
                    | UNAuthorizationOptions::Badge,
                &answered,
            );
    });
}

pub fn post(note: &Note) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&note.title));
    content.setBody(&NSString::from_str(&note.body));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    content.setTargetContentIdentifier(Some(&NSString::from_str(note.open.as_str())));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&note.id),
        &content,
        None,
    );
    let id = note.id.clone();
    let added = RcBlock::new(move |error: *mut NSError| {
        if let Some(error) = unsafe { error.as_ref() } {
            tracing::warn!(id, error = %error.localizedDescription(), "a banner was not posted");
        } else {
            tracing::debug!(id, "a banner was posted");
        }
    });
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.addNotificationRequest_withCompletionHandler(&request, Some(&added));
    // A post succeeds whatever the permission, and the person can change it
    // in System Settings at any time: read it again with each banner.
    refresh();
}

/// Reads whether the person allows banners, for the menu.
pub fn refresh() {
    let settings = RcBlock::new(|settings: NonNull<UNNotificationSettings>| {
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let on = matches!(
            status,
            UNAuthorizationStatus::Authorized
                | UNAuthorizationStatus::Provisional
                | UNAuthorizationStatus::Ephemeral
        );
        if let Some(shell) = SHELL.get() {
            shell.send(Command::Notifications(on));
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&settings);
}

pub fn retract(ids: &[String]) {
    let ids: Vec<Retained<NSString>> = ids.iter().map(|id| NSString::from_str(id)).collect();
    let ids = NSArray::from_retained_slice(&ids);
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
    center.removePendingNotificationRequestsWithIdentifiers(&ids);
}
