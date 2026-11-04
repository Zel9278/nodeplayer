// Standalone AppKit regression: run on the main thread, without hardware input.
// See docs/development.md for the compiler command from the repository root.
#include "cocoa_input.mm"
#include <cassert>
#include <cstdio>

int main() {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [app finishLaunching];
        auto window = [] {
            NSWindow *result = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 80, 80)
                styleMask:NSWindowStyleMaskBorderless backing:NSBackingStoreBuffered defer:NO];
            result.releasedWhenClosed = NO;
            result.alphaValue = 0;
            result.ignoresMouseEvents = YES;
            return result;
        };
        NSWindow *graph = window(), *root_a = window(), *root_b = window();
        graph.ignoresMouseEvents = NO;
        NSWindow *menu_a = window(), *menu_b = window();
        menu_a.ignoresMouseEvents = NO;
        menu_b.ignoresMouseEvents = NO;
        [root_a addChildWindow:menu_a ordered:NSWindowAbove];
        [root_b addChildWindow:menu_b ordered:NSWindowAbove];
        void *a = np_capture_input_create((__bridge void *)root_a.contentView);
        void *b = np_capture_input_create((__bridge void *)root_b.contentView);
        assert(a && b);
        __block unsigned host_calls = 0, a_calls = 0, nested_calls = 0, b_calls = 0;
        id host = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown
            handler:^NSEvent *(NSEvent *event) { ++host_calls; return event; }];
        void *previous = np_capture_input_begin(a);
        id monitor_a = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown
            handler:^NSEvent *(NSEvent *event) { ++a_calls; return event; }];
        void *nested = np_capture_input_begin(b);
        id monitor_b = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown
            handler:^NSEvent *(NSEvent *event) { ++b_calls; return event; }];
        np_capture_input_end(nested);
        id monitor_nested = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskLeftMouseDown
            handler:^NSEvent *(NSEvent *event) { ++nested_calls; return event; }];
        np_capture_input_end(previous);
        // A normal popout's monitor was registered outside captured input and must
        // continue seeing the original graph event, unchanged and exactly once.
        auto click = [&](NSWindow *target) {
            NSEvent *event = [NSEvent mouseEventWithType:NSEventTypeLeftMouseDown
                location:NSMakePoint(17, 23) modifierFlags:0 timestamp:0
                windowNumber:target.windowNumber context:nil eventNumber:0 clickCount:1 pressure:1];
            [app postEvent:event atStart:YES];
            bool delivered = false;
            for (unsigned i = 0; i < 128; ++i) {
                NSEvent *next = [app nextEventMatchingMask:NSEventMaskAny untilDate:NSDate.distantPast
                    inMode:NSDefaultRunLoopMode dequeue:YES];
                if (!next) break;
                if (next.type == NSEventTypeLeftMouseDown) {
                    assert(next.window == target);
                    assert(NSEqualPoints(next.locationInWindow, NSMakePoint(17, 23)));
                    delivered = true;
                }
                [app sendEvent:next];
            }
            assert(delivered);
        };
        click(graph);
        assert(host_calls == 1 && a_calls == 1 && b_calls == 1 && nested_calls == 1);
        [menu_a orderBack:nil];
        assert(menu_a.visible);
        click(graph);
        assert(host_calls == 2 && a_calls == 1 && b_calls == 2 && nested_calls == 1);
        click(menu_a);
        assert(host_calls == 3 && a_calls == 2 && b_calls == 3 && nested_calls == 2);
        [menu_b orderBack:nil];
        click(menu_b);
        assert(host_calls == 4 && a_calls == 2 && b_calls == 4 && nested_calls == 2);
        [menu_a orderOut:nil];
        click(graph);
        assert(host_calls == 5 && a_calls == 3 && b_calls == 4 && nested_calls == 3);
        np_capture_input_destroy(a);
        click(graph);
        assert(host_calls == 6 && a_calls == 3 && b_calls == 4 && nested_calls == 3);
        // A stale plugin monitor must never dereference its former editor after drop.
        np_capture_input_destroy(b);
        [menu_b orderOut:nil];
        click(graph);
        assert(host_calls == 7 && b_calls == 4);
        for (id monitor in @[host, monitor_a, monitor_b, monitor_nested])
            [NSEvent removeMonitor:monitor];
        for (NSWindow *owned in @[menu_a, menu_b, root_a, root_b, graph]) [owned close];
        std::puts("captured menu monitor routing: passed");
    }
}
