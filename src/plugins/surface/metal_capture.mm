// Read back a plugin's completed Metal drawables without a screen capture permission,
// window activation, or a GPU wait on the plugin's UI thread.
#import <Cocoa/Cocoa.h>
#import <MetalKit/MetalKit.h>
#import <QuartzCore/QuartzCore.h>
#import <objc/runtime.h>

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <memory>
#include <mutex>
#include <unordered_map>
#include <vector>

struct NpMetalFrame {
    uint32_t width, height, native_width, native_height, bgra;
    uint64_t sequence;
};

namespace {
constexpr uint64_t max_pixels = 16 * 1024 * 1024;
struct Image {
    NpMetalFrame info;
    std::vector<uint8_t> bytes;
};
struct Capture {
    std::atomic<bool> active{true}, pending{false};
    std::atomic<uint32_t> failure{0};
    std::atomic<uint64_t> next_capture{0};
    std::mutex mutex;
    std::shared_ptr<const Image> image;
    uint64_t sequence = 0; // Only one GPU readback may complete at a time.
    __strong id<MTLCommandQueue> queue;
};
std::mutex registry_mutex;
std::unordered_map<void *, std::weak_ptr<Capture>> registry;
}

// No ivars: replacing an ordinary CAMetalLayer's class leaves its allocation/layout
// intact. Only layers owned by an active adapter enter the registry. Other layers,
// custom CAMetalLayer subclasses, and the application's renderer are untouched.
@interface NodeplayerReadbackMetalLayer : CAMetalLayer
@end

@implementation NodeplayerReadbackMetalLayer
- (id<CAMetalDrawable>)nextDrawable {
    id<CAMetalDrawable> drawable = [super nextDrawable];
    if (!drawable) return drawable;
    std::shared_ptr<Capture> capture;
    {
        std::lock_guard<std::mutex> lock(registry_mutex);
        auto found = registry.find((__bridge void *)self);
        if (found != registry.end()) capture = found->second.lock();
    }
    if (!capture || !capture->active.load()) return drawable;
    auto now = std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
    if (uint64_t(now) < capture->next_capture.load() || capture->pending.exchange(true))
        return drawable;
    capture->next_capture.store(uint64_t(now) + 1000000000 / 240);
    id<MTLTexture> texture = drawable.texture;
    const auto width = texture.width, height = texture.height;
    const auto format = texture.pixelFormat;
    const bool bgra = format == MTLPixelFormatBGRA8Unorm || format == MTLPixelFormatBGRA8Unorm_sRGB;
    const bool rgba = format == MTLPixelFormatRGBA8Unorm || format == MTLPixelFormatRGBA8Unorm_sRGB;
    const CGSize native = self.bounds.size;
    if (!width || !height || width > 8192 || height > 8192 || width * height > max_pixels
        || (!bgra && !rgba) || !std::isfinite(native.width) || !std::isfinite(native.height)
        || native.width <= 0 || native.height <= 0 || native.width > 8192 || native.height > 8192) {
        capture->failure.store(1);
        capture->pending.store(false);
        return drawable;
    }
    const NpMetalFrame info{uint32_t(width), uint32_t(height),
        uint32_t(std::round(native.width)), uint32_t(std::round(native.height)), uint32_t(bgra), 0};
    // Presentation is later than this drawable's rendering. Its callback submits the
    // blit; retaining the drawable until blit completion prevents pool reuse mid-copy.
    // The callback captures the texture, not the drawable that owns the callback.
    [drawable addPresentedHandler:^(id<MTLDrawable> presented) {
      @autoreleasepool {
        if (!capture->active.load()) { capture->pending.store(false); return; }
        id<MTLDrawable> held_drawable = presented;
        const auto stride = ((width * 4 + 255) / 256) * 256;
        id<MTLBuffer> buffer = [texture.device newBufferWithLength:stride * height
                                                        options:MTLResourceStorageModeShared];
        id<MTLCommandBuffer> command = [capture->queue commandBuffer];
        id<MTLBlitCommandEncoder> blit = [command blitCommandEncoder];
        if (!buffer || !command || !blit) {
            capture->failure.store(2); capture->pending.store(false); return;
        }
        [blit copyFromTexture:texture sourceSlice:0 sourceLevel:0
                 sourceOrigin:MTLOriginMake(0, 0, 0) sourceSize:MTLSizeMake(width, height, 1)
                     toBuffer:buffer destinationOffset:0 destinationBytesPerRow:stride
          destinationBytesPerImage:stride * height];
        [blit endEncoding];
        [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
          @autoreleasepool {
            if (capture->active.load() && completed.status == MTLCommandBufferStatusCompleted) {
                auto image = std::make_shared<Image>();
                image->info = info;
                image->info.sequence = ++capture->sequence;
                image->bytes.resize(width * height * 4);
                const auto *source = static_cast<const uint8_t *>(buffer.contents);
                for (size_t y = 0; y < height; ++y)
                    std::memcpy(image->bytes.data() + y * width * 4, source + y * stride, width * 4);
                std::lock_guard<std::mutex> lock(capture->mutex);
                capture->image = std::move(image);
            } else if (capture->active.load()) {
                capture->failure.store(3);
            }
            (void)held_drawable.drawableID;
            capture->pending.store(false);
          }
        }];
        [command commit];
      }
    }];
    return drawable;
}
@end

namespace {
struct CaptureHandle {
    std::shared_ptr<Capture> capture;
    __strong CAMetalLayer *layer;
    __strong NSView *gpu_view;
    __strong MTKView *metal_view;
    bool framebuffer_only;
};

NSView *find_metal_view(NSView *view, NSView *root, unsigned depth) {
    if (depth > 8) return nil;
    if ([view.layer isMemberOfClass:[CAMetalLayer class]]) {
        const NSRect bounds = [view convertRect:view.bounds toView:root];
        const NSRect area = root.bounds;
        // A full-editor drawable can replace its bitmap directly. Mixed GPU/CPU view
        // composition needs a separate adapter; never drop controls outside this layer.
        if (std::abs(bounds.origin.x - area.origin.x) < 0.5
            && std::abs(bounds.origin.y - area.origin.y) < 0.5
            && std::abs(bounds.size.width - area.size.width) < 0.5
            && std::abs(bounds.size.height - area.size.height) < 0.5) return view;
    }
    unsigned count = 0;
    for (NSView *child in view.subviews) {
        if (++count > 64) break;
        NSView *found = find_metal_view(child, root, depth + 1);
        if (found) return found;
    }
    return nil;
}
}

extern "C" void *np_metal_capture_create(void *root_view) {
    @autoreleasepool {
        if (![NSThread isMainThread]) return nullptr;
        NSView *view = find_metal_view((__bridge NSView *)root_view,
                                      (__bridge NSView *)root_view, 0);
        if (!view || class_getInstanceSize([CAMetalLayer class])
                     != class_getInstanceSize([NodeplayerReadbackMetalLayer class])) return nullptr;
        CAMetalLayer *layer = (CAMetalLayer *)view.layer;
        id<MTLCommandQueue> queue = [layer.device newCommandQueue];
        if (!queue) return nullptr;
        auto capture = std::make_shared<Capture>();
        capture->queue = queue;
        MTKView *metal_view = [view isKindOfClass:[MTKView class]] ? (MTKView *)view : nil;
        const bool framebuffer_only = metal_view ? metal_view.framebufferOnly : layer.framebufferOnly;
        if (metal_view) metal_view.framebufferOnly = NO;
        else layer.framebufferOnly = NO;
        {
            std::lock_guard<std::mutex> lock(registry_mutex);
            registry[(__bridge void *)layer] = capture;
            object_setClass(layer, [NodeplayerReadbackMetalLayer class]);
        }
        return new CaptureHandle{std::move(capture), layer, view, metal_view, framebuffer_only};
    }
}

extern "C" bool np_metal_capture_current(void *opaque, void *root_view) {
    auto *handle = static_cast<CaptureHandle *>(opaque);
    NSView *root = (__bridge NSView *)root_view;
    return handle->gpu_view.layer == handle->layer && [handle->gpu_view isDescendantOf:root];
}

// Zero means no image yet. The required byte count is returned even when the provided
// buffer is too short. A snapshot stays alive independently of concurrent publication.
extern "C" size_t np_metal_capture_copy(void *opaque, NpMetalFrame *info,
                                        uint8_t *destination, size_t capacity) {
    auto *handle = static_cast<CaptureHandle *>(opaque);
    std::shared_ptr<const Image> image;
    {
        std::unique_lock<std::mutex> lock(handle->capture->mutex, std::try_to_lock);
        if (!lock.owns_lock()) return 0;
        image = handle->capture->image;
    }
    if (!image) return 0;
    *info = image->info;
    if (destination && capacity >= image->bytes.size())
        std::memcpy(destination, image->bytes.data(), image->bytes.size());
    return image->bytes.size();
}

extern "C" uint32_t np_metal_capture_failure(void *opaque) {
    return static_cast<CaptureHandle *>(opaque)->capture->failure.load();
}

extern "C" void np_metal_capture_destroy(void *opaque) {
    auto *handle = static_cast<CaptureHandle *>(opaque);
    handle->capture->active.store(false);
    {
        std::lock_guard<std::mutex> lock(registry_mutex);
        registry.erase((__bridge void *)handle->layer);
        if (object_getClass(handle->layer) == [NodeplayerReadbackMetalLayer class])
            object_setClass(handle->layer, [CAMetalLayer class]);
    }
    if (handle->metal_view) handle->metal_view.framebufferOnly = handle->framebuffer_only;
    else handle->layer.framebufferOnly = handle->framebuffer_only;
    // Pending callbacks retain only Metal resources and private state, never plugin views.
    delete handle;
}
