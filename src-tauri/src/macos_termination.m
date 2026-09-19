#import <AppKit/AppKit.h>
#import <objc/runtime.h>

static int (*requestQuit)(void);

static NSApplicationTerminateReply shouldTerminate(id delegate, SEL command, NSApplication *sender) {
    // Cancel this native request while the existing asynchronous save decision runs.
    return requestQuit && requestQuit() ? NSTerminateCancel : NSTerminateNow;
}

int risunest_install_termination_handler(int (*callback)(void)) {
    if (![NSThread isMainThread] || !NSApp.delegate || !callback) return 0;
    Class delegateClass = object_getClass(NSApp.delegate);
    SEL selector = @selector(applicationShouldTerminate:);
    // Never replace a runtime implementation introduced by a dependency update.
    if (class_getInstanceMethod(delegateClass, selector)) return 0;
    struct objc_method_description method = protocol_getMethodDescription(
        @protocol(NSApplicationDelegate), selector, NO, YES);
    if (!method.types) return 0;
    requestQuit = callback;
    if (!class_addMethod(delegateClass, selector, (IMP)shouldTerminate, method.types)) {
        requestQuit = NULL;
        return 0;
    }
    return 1;
}
