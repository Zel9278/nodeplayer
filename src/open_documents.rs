//! macOS hands files opened from the Finder (or dropped on the Dock icon) to an app as an
//! "open documents" Apple event, not on the command line, and to the running app when there
//! is one. They go to the window like files from a second start ([`crate::instance::deliver`]).
//!
//! NSApplication puts its own handlers in place while it finishes launching and sends the
//! first files right after, so ours replaces them at "will finish launching".

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{AllocAnyThread, define_class, msg_send, sel};
use objc2_foundation::{NSObject, NSString};

/// `kCoreEventClass` ('aevt'), `kAEOpenDocuments` ('odoc') and `keyDirectObject` ('----').
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

define_class!(
    // SAFETY: a plain NSObject subclass; its methods only read their arguments.
    #[unsafe(super(NSObject))]
    #[name = "NodeplayerOpenDocuments"]
    struct Handler;

    impl Handler {
        #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
        fn handle_open_documents(&self, event: &AnyObject, _reply: &AnyObject) {
            let files = files(event);
            log::debug!("opened from the system: {files:?}");
            crate::instance::deliver(files);
        }

        #[unsafe(method(appWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: &AnyObject) {
            register(self);
        }
    }
);

impl Handler {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Starts taking open-documents events. Call before the event loop runs.
pub fn install() {
    let handler = Handler::new();
    register(&handler);
    unsafe {
        if let Some(class) = AnyClass::get(c"NSNotificationCenter") {
            let center: *mut AnyObject = msg_send![class, defaultCenter];
            if let Some(center) = center.as_ref() {
                let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
                let _: () = msg_send![
                    center,
                    addObserver: &*handler,
                    selector: sel!(appWillFinishLaunching:),
                    name: &*name,
                    object: std::ptr::null::<AnyObject>()
                ];
            }
        }
    }
    // Lives as long as the app: neither the event manager nor the center retains it.
    std::mem::forget(handler);
}

fn register(handler: &Handler) {
    let Some(class) = AnyClass::get(c"NSAppleEventManager") else { return };
    unsafe {
        let manager: *mut AnyObject = msg_send![class, sharedAppleEventManager];
        if let Some(manager) = manager.as_ref() {
            let _: () = msg_send![
                manager,
                setEventHandler: handler,
                andSelector: sel!(handleOpenDocuments:withReplyEvent:),
                forEventClass: CORE_EVENT_CLASS,
                andEventID: OPEN_DOCUMENTS
            ];
        }
    }
}

/// The files an open-documents event names: a list of file references (or just one).
fn files(event: &AnyObject) -> Vec<PathBuf> {
    unsafe {
        let list: Option<Retained<AnyObject>> = msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
        let Some(list) = list else { return Vec::new() };
        let count: isize = msg_send![&*list, numberOfItems];
        if count == 0 {
            return path_of(&list).into_iter().collect();
        }
        (1..=count)
            .filter_map(|i| {
                let item: Option<Retained<AnyObject>> = msg_send![&*list, descriptorAtIndex: i];
                let item = item?;
                path_of(&item)
            })
            .collect()
    }
}

fn path_of(descriptor: &AnyObject) -> Option<PathBuf> {
    unsafe {
        let url: Option<Retained<AnyObject>> = msg_send![descriptor, fileURLValue];
        let url = url?;
        let path: Option<Retained<NSString>> = msg_send![&*url, path];
        Some(PathBuf::from(path?.to_string()))
    }
}
