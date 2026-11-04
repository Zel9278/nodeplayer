// Local plugin menus can monitor every AppKit click in their process. A captured
// menu sees the graph's original click before the host can forward it, and may
// mistake it for an outside click. Let graph input/cancellation own that decision.
#import <Cocoa/Cocoa.h>
#import <objc/runtime.h>

#include <memory>

namespace {
struct CaptureInput {
    __weak NSWindow *root;
    bool active = true;
};
using Owner = std::shared_ptr<CaptureInput>;
thread_local Owner *input_owner = nullptr;
using AddMonitor = id (*)(id, SEL, NSEventMask, NSEvent *(^)(NSEvent *));
AddMonitor original_add_monitor = nullptr;

bool pointer_event(NSEventType type) {
    switch (type) {
        case NSEventTypeLeftMouseDown: case NSEventTypeLeftMouseUp:
        case NSEventTypeRightMouseDown: case NSEventTypeRightMouseUp:
        case NSEventTypeOtherMouseDown: case NSEventTypeOtherMouseUp:
        case NSEventTypeMouseMoved: case NSEventTypeLeftMouseDragged:
        case NSEventTypeRightMouseDragged: case NSEventTypeOtherMouseDragged:
        case NSEventTypeScrollWheel:
            return true;
        default:
            return false;
    }
}

bool belongs_to(NSWindow *window, NSWindow *root) {
    for (unsigned depth = 0; window && depth < 17; ++depth) {
        if (window == root) return true;
        window = window.parentWindow;
    }
    return false;
}

bool has_menu(NSWindow *root, unsigned depth = 0) {
    if (depth >= 16) return false;
    for (NSWindow *child in root.childWindows) {
        if (child.visible && (!child.ignoresMouseEvents || has_menu(child, depth + 1)))
            return true;
    }
    return false;
}

id add_monitor(id cls, SEL selector, NSEventMask mask, NSEvent *(^handler)(NSEvent *)) {
    if (!input_owner || !handler)
        return original_add_monitor(cls, selector, mask, handler);
    Owner owner = *input_owner;
    return original_add_monitor(cls, selector, mask, ^NSEvent *(NSEvent *event) {
        // A plugin may leave a monitor installed during deferred GUI destruction.
        // Never call its captured GUI pointers after this adapter has gone away.
        if (!owner->active) return event;
        NSWindow *root = owner->root;
        if (root && pointer_event(event.type) && !belongs_to(event.window, root)
            && has_menu(root))
            return event;
        return handler(event);
    });
}
}

extern "C" void *np_capture_input_create(void *view) {
    if (![NSThread isMainThread] || !view) return nullptr;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Method method = class_getClassMethod([NSEvent class],
            @selector(addLocalMonitorForEventsMatchingMask:handler:));
        original_add_monitor = reinterpret_cast<AddMonitor>(
            method_setImplementation(method, reinterpret_cast<IMP>(add_monitor)));
    });
    auto owner = std::make_shared<CaptureInput>();
    owner->root = [(__bridge NSView *)view window];
    return new Owner(std::move(owner));
}

extern "C" void *np_capture_input_begin(void *handle) {
    auto previous = input_owner;
    input_owner = static_cast<Owner *>(handle);
    return previous;
}

extern "C" void np_capture_input_end(void *previous) {
    input_owner = static_cast<Owner *>(previous);
}

extern "C" void np_capture_input_destroy(void *handle) {
    auto owner = static_cast<Owner *>(handle);
    (*owner)->active = false;
    delete owner;
}
