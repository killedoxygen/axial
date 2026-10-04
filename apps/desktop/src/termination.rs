use objc2::{
    MainThreadMarker,
    runtime::{AnyObject, ClassBuilder, Sel},
    sel,
};
use objc2_app_kit::{NSApplication, NSApplicationTerminateReply};
use std::{
    cell::RefCell,
    io,
    rc::{Rc, Weak},
};

thread_local! {
    static APPLICATION: RefCell<Weak<tauri::AppHandle>> = const { RefCell::new(Weak::new()) };
}

// Retain routing only while the event loop can receive ExitRequested. Tauri's
// exit() falls back to process::exit after that receiver has disappeared.
pub struct Guard {
    _application: Rc<tauri::AppHandle>,
}

pub fn install(handle: &tauri::AppHandle) -> io::Result<Guard> {
    let invalid = || io::Error::other("The native termination guard could not be configured.");
    let main_thread = MainThreadMarker::new().ok_or_else(invalid)?;
    let application = NSApplication::sharedApplication(main_thread);
    let delegate = application.delegate().ok_or_else(invalid)?;
    let object: &AnyObject = (*delegate).as_ref();
    let previous = object.class();
    let selector = sel!(applicationShouldTerminate:);
    if previous.name() != c"TaoAppDelegateParent" || previous.instance_method(selector).is_some() {
        return Err(invalid());
    }
    let mut builder =
        ClassBuilder::new(c"AxialTerminationDelegate", previous).ok_or_else(invalid)?;
    // This is AppKit's exact delegate signature, including NSUInteger reply.
    unsafe {
        builder.add_method(selector, should_terminate as extern "C" fn(_, _, _) -> _);
    }
    let guarded = builder.register();
    if guarded.instance_size() != previous.instance_size() {
        return Err(invalid());
    }
    // Main-thread-only installation, before run_return: a zero-ivar subclass
    // adds one absent method and inherits all existing Tao delegate behavior.
    let replaced = unsafe { AnyObject::set_class(object, guarded) };
    assert!(std::ptr::eq(replaced, previous));
    application.setDelegate(Some(&delegate));
    let retained = Rc::new(handle.clone());
    APPLICATION.with(|current| *current.borrow_mut() = Rc::downgrade(&retained));
    Ok(Guard {
        _application: retained,
    })
}

extern "C" fn should_terminate(
    _: &AnyObject,
    _: Sel,
    _: &NSApplication,
) -> NSApplicationTerminateReply {
    let application = APPLICATION.with(|current| current.borrow().upgrade());
    if let Some(application) = application {
        application.exit(0);
    }
    // Let the existing event-loop owner decide and finish cleanup. Even an
    // allowed close cannot authorize AppKit to skip post-loop joins.
    NSApplicationTerminateReply::TerminateCancel
}
