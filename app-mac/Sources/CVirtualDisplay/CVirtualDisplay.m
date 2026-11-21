#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#include "CVirtualDisplay.h"

// Private CoreGraphics classes (as declared by DeskPad and others). Only
// messaged through NSClassFromString results, so nothing links against them.
@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(NSUInteger)width height:(NSUInteger)height refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic) unsigned int hiDPI;
@property(retain, nonatomic) NSArray *modes;
@end

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) dispatch_queue_t queue;
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@property(nonatomic) unsigned int serialNum;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(readonly, nonatomic) CGDirectDisplayID displayID;
@end

bool glass_vd_available(void) {
    return NSClassFromString(@"CGVirtualDisplay") != nil && NSClassFromString(@"CGVirtualDisplayDescriptor") != nil &&
           NSClassFromString(@"CGVirtualDisplaySettings") != nil && NSClassFromString(@"CGVirtualDisplayMode") != nil;
}

void *glass_vd_create(const char *name, uint32_t mode_w, uint32_t mode_h, double hz, bool hidpi,
                      uint32_t *display_id) {
    @autoreleasepool {
        if (!glass_vd_available() || mode_w == 0 || mode_h == 0) return NULL;
        uint32_t pw = hidpi ? mode_w * 2 : mode_w;
        uint32_t ph = hidpi ? mode_h * 2 : mode_h;

        CGVirtualDisplayDescriptor *d = [[NSClassFromString(@"CGVirtualDisplayDescriptor") alloc] init];
        d.queue = dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0);
        d.name = [NSString stringWithUTF8String:name ? name : "Glass"];
        d.maxPixelsWide = pw;
        d.maxPixelsHigh = ph;
        // About 110 points per inch, like a desktop monitor.
        d.sizeInMillimeters = CGSizeMake(mode_w * 25.4 / 110.0, mode_h * 25.4 / 110.0);
        d.vendorID = 0x676c; // "gl"
        d.productID = 0x6173; // "as"
        d.serialNum = (unsigned int)(mode_w << 16 | mode_h);

        CGVirtualDisplay *vd = [[NSClassFromString(@"CGVirtualDisplay") alloc] initWithDescriptor:d];
        if (vd == nil) return NULL;
        CGVirtualDisplaySettings *s = [[NSClassFromString(@"CGVirtualDisplaySettings") alloc] init];
        s.hiDPI = hidpi ? 1 : 0;
        CGVirtualDisplayMode *m = [[NSClassFromString(@"CGVirtualDisplayMode") alloc] initWithWidth:mode_w
                                                                                              height:mode_h
                                                                                         refreshRate:hz];
        s.modes = @[ m ];
        if (![vd applySettings:s]) return NULL;
        if (display_id) *display_id = vd.displayID;
        return (__bridge_retained void *)vd;
    }
}

void glass_vd_destroy(void *handle) {
    if (handle == NULL) return;
    @autoreleasepool {
        CGVirtualDisplay *vd = (__bridge_transfer CGVirtualDisplay *)handle;
        (void)vd; // released here: the display goes away
    }
}
