#import <Cocoa/Cocoa.h>
#import <CoreGraphics/CoreGraphics.h>

@class CGVirtualDisplayDescriptor;
@interface CGVirtualDisplayMode : NSObject
@property(readonly, nonatomic) CGFloat refreshRate;
@property(readonly, nonatomic) NSUInteger width;
@property(readonly, nonatomic) NSUInteger height;
- (instancetype)initWithWidth:(NSUInteger)arg1
                       height:(NSUInteger)arg2
                  refreshRate:(CGFloat)arg3;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic) unsigned int hiDPI;
@property(retain, nonatomic) NSArray<CGVirtualDisplayMode *> *modes;
- (instancetype)init;
@end

@interface CGVirtualDisplay : NSObject
@property(readonly, nonatomic) CGDirectDisplayID displayID;
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)arg1;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)arg1;
@end

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int serialNum;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@property(copy, nonatomic) void (^terminationHandler)(id, CGVirtualDisplay *);
- (instancetype)init;
- (nullable dispatch_queue_t)dispatchQueue;
- (void)setDispatchQueue:(dispatch_queue_t)arg1;
@end

struct DisplayObject {
  uint32_t displayId;
  unsigned int width;
  unsigned int height;
};

class VDisplay {
  /*
  public:
    static Napi::Function GetClass(Napi::Env);
    VDisplay(const Napi::CallbackInfo &info);

  */
public:
  DisplayObject CreateVirtualDisplay(unsigned int width, unsigned int height,
                                     double refreshRate, bool hiDPI,
                                     char *displayNameStr, int ppi,
                                     bool useMirror);
  DisplayObject CloneVirtualDisplay(char *displayNameStr, bool useMirror);
  bool DestroyVirtualDisplay();

private:
  CGVirtualDisplay *_display;
  CGVirtualDisplayDescriptor *_descriptor;
  CGVirtualDisplaySettings *_settings;

  void InitializeDescriptor(NSString *displayName, unsigned int width,
                            unsigned int height, int ppi);
  void InitializeSettings(unsigned int width, unsigned int height,
                          CGFloat refreshRate, bool hiDPI);
  DisplayObject CreateDisplayObject(unsigned int width, unsigned int height);
  DisplayObject NullDisplayObject();

  int Clamp(int value, int low, int high) {
    return (value < low) ? low : ((value > high) ? high : value);
  }
};

/*
VDisplay::VDisplay(const Napi::CallbackInfo &info) : ObjectWrap(info) {}

Napi::Function VDisplay::GetClass(Napi::Env env) {
  return DefineClass(
      env, "VDisplay",
      {
          InstanceMethod("createVirtualDisplay",
                         &VDisplay::CreateVirtualDisplay),
          InstanceMethod("cloneVirtualDisplay", &VDisplay::CloneVirtualDisplay),
          InstanceMethod("destroyVirtualDisplay",
                         &VDisplay::DestroyVirtualDisplay),
      });
}
*/

void VDisplay::InitializeDescriptor(NSString *displayName, unsigned int width,
                                    unsigned int height, int ppi) {
  _descriptor = [[CGVirtualDisplayDescriptor alloc] init];
  _descriptor.name = displayName;
  _descriptor.maxPixelsWide = width;
  _descriptor.maxPixelsHigh = height;

  double ratio = 25.4 / ppi;
  _descriptor.sizeInMillimeters = CGSizeMake(width * ratio, height * ratio);
  _descriptor.productID = 0xeeee + width + height + ppi;
  _descriptor.vendorID = 0xeeee;
  _descriptor.serialNum = 0x0001;
}

void VDisplay::InitializeSettings(unsigned int width, unsigned int height,
                                  CGFloat refreshRate, bool hiDPI) {
  _settings = [[CGVirtualDisplaySettings alloc] init];
  _settings.hiDPI = hiDPI ? 1 : 0;

  CGVirtualDisplayMode *mode =
      [[CGVirtualDisplayMode alloc] initWithWidth:width
                                           height:height
                                      refreshRate:refreshRate];
  if (hiDPI) {
    CGVirtualDisplayMode *lowResMode =
        [[CGVirtualDisplayMode alloc] initWithWidth:width / 2
                                             height:height / 2
                                        refreshRate:refreshRate];
    _settings.modes = @[ mode, lowResMode ];
    [mode release];
    [lowResMode release];
  } else {
    _settings.modes = @[ mode ];
    [mode release];
  }
}

DisplayObject VDisplay::CreateDisplayObject(unsigned int width,
                                            unsigned int height) {
  struct DisplayObject obj;
  obj.displayId = _display.displayID;
  obj.width = width;
  obj.height = height;
  return obj;
}

DisplayObject VDisplay::NullDisplayObject() {
  struct DisplayObject obj;
  obj.displayId = 0;
  obj.width = 0;
  obj.height = 0;
  return obj;
}

DisplayObject VDisplay::CreateVirtualDisplay(unsigned int width,
                                             unsigned int height,
                                             double refreshRate, bool hiDPI,
                                             char *displayNameStr, int ppi,
                                             bool useMirror) {
  // Clean up existing display if any
  if (_display) {
    [_descriptor release];
    _descriptor = nil;
    [_settings release];
    _settings = nil;
    [_display release];
    _display = nil;
  }

  // Params [width, height, refreshRate, hiDPI, displayName, ppi, useMirror]

  refreshRate = Clamp(refreshRate, 30, 60);
  ppi = Clamp(refreshRate, 72, 300);

  NSString *displayName = [NSString stringWithUTF8String:displayNameStr];
  if (!displayName) {
    displayName = @"Virtual Display";
  }

  // store current main display id and bounds
  // CGRect mainBounds = CGDisplayBounds(CGMainDisplayID());
  uint32_t mainDisplay = CGMainDisplayID();
  NSLog(@"Previous Main display ID: %d", mainDisplay);

  InitializeDescriptor(displayName, width, height, ppi);
  if (!_descriptor) {
    NSLog(@"Failed to create display descriptor");
    return NullDisplayObject();
  }

  _display = [[CGVirtualDisplay alloc] initWithDescriptor:_descriptor];
  if (!_display) {
    NSLog(@"Failed to create virtual display");
    return NullDisplayObject();
  }

  InitializeSettings(width, height, refreshRate, hiDPI);
  [_display applySettings:_settings];

  // postprocess start
  uint32_t newMainDisplayID = CGMainDisplayID();
  NSLog(@"Current Main Display after virtual display creation: %d",
        newMainDisplayID);

  CGDisplayConfigRef config;
  CGBeginDisplayConfiguration(&config);
  if (newMainDisplayID == _display.displayID &&
      newMainDisplayID != mainDisplay) {
    NSLog(@"Unintended case 1: Virtual display set as main display => restore "
          @"Primary Display as main display");
    CGConfigureDisplayOrigin(config, mainDisplay, 0, 0);
  }

  // if Primary Display is Mirroring Virtual Display, disable mirror mode
  uint32_t displayId = CGDisplayMirrorsDisplay(mainDisplay);
  NSLog(@"Mirror source of Primary Display is: %d", displayId);
  if (displayId == _display.displayID) {
    NSLog(@"Unintended case 2: Primary display is mirroring virtual display => "
          @"disable mirror mode");
    CGConfigureDisplayMirrorOfDisplay(config, displayId, kCGNullDirectDisplay);
  }
  CGCompleteDisplayConfiguration(config, kCGConfigureForAppOnly);

  boolean_t isMirror = CGDisplayIsInMirrorSet(_display.displayID);
  NSLog(@"Virtual Display is in mirror set: %d", isMirror);
  CGBeginDisplayConfiguration(&config);
  if (useMirror) {
    if (isMirror == 0) {
      NSLog(@"Enable Virtual Display mirror mode");
      // set mirror mode
      CGError err = CGConfigureDisplayMirrorOfDisplay(
          config, _display.displayID, mainDisplay);
      if (err != kCGErrorSuccess) {
        NSLog(@"Failed to enable mirror mode: %d", err);
      }
    }
  } else {
    if (isMirror == 1) {
      NSLog(@"Disable Virtual Display mirror mode");
      // if already in mirror mode, disable mirror mode
      CGError err = CGConfigureDisplayMirrorOfDisplay(
          config, _display.displayID, kCGNullDirectDisplay);
      if (err != kCGErrorSuccess) {
        NSLog(@"Failed to disable mirror mode: %d", err);
      }
    }
  }
  CGCompleteDisplayConfiguration(config, kCGConfigureForAppOnly);
  // postprocess end

  NSLog(@"Virtual display created with ID: %d", _display.displayID);
  return CreateDisplayObject(width, height);
}

DisplayObject VDisplay::CloneVirtualDisplay(char *displayNameStr,
                                            bool useMirror) {
  // Clean up existing display if any
  if (_display) {
    [_descriptor release];
    _descriptor = nil;
    [_settings release];
    _settings = nil;
    [_display release];
    _display = nil;
  }

  // Params [displayName, useMirror]
  NSString *displayName = [NSString stringWithUTF8String:displayNameStr];
  if (!displayName || displayName.length == 0) {
    displayName = @"Virtual Display";
  }

  CGDirectDisplayID mainDisplay = CGMainDisplayID();
  CGDisplayModeRef displayMode = CGDisplayCopyDisplayMode(mainDisplay);

  NSScreen *mainScreen = [NSScreen mainScreen];
  CGFloat backingScaleFactor = [mainScreen backingScaleFactor];

  unsigned int width =
      CGDisplayModeGetPixelWidth(displayMode) / backingScaleFactor;
  unsigned int height =
      CGDisplayModeGetPixelHeight(displayMode) / backingScaleFactor;
  CGFloat refreshRate = CGDisplayModeGetRefreshRate(displayMode);

  CGSize screenSize = CGDisplayScreenSize(mainDisplay);
  float dpi = CGDisplayPixelsWide(mainDisplay) / (screenSize.width / 25.4);
  // increase DPI for retina display
  bool isHiDPI = (dpi > 200);

  InitializeDescriptor(displayName, width, height, dpi);
  _descriptor.productID = CGDisplayModelNumber(mainDisplay) + 1;
  _descriptor.vendorID = CGDisplayVendorNumber(mainDisplay);

  _display = [[CGVirtualDisplay alloc] initWithDescriptor:_descriptor];
  InitializeSettings(width, height, refreshRate, isHiDPI);
  [_display applySettings:_settings];

  CFRelease(displayMode);

  // postprocess start
  uint32_t newMainDisplayID = CGMainDisplayID();
  NSLog(@"Current Main Display after virtual display creation: %d",
        newMainDisplayID);

  CGDisplayConfigRef config;
  CGBeginDisplayConfiguration(&config);
  if (newMainDisplayID == _display.displayID &&
      newMainDisplayID != mainDisplay) {
    NSLog(@"Unintended case 1: Virtual display set as main display => restore "
          @"Primary Display as main display");
    CGConfigureDisplayOrigin(config, mainDisplay, 0, 0);
  }

  // if Primary Display is Mirroring Virtual Display, disable mirror mode
  uint32_t displayId = CGDisplayMirrorsDisplay(mainDisplay);
  NSLog(@"Mirror source of Primary Display is: %d", displayId);
  if (displayId == _display.displayID) {
    NSLog(@"Unintended case 2: Primary display is mirroring virtual display => "
          @"disable mirror mode");
    CGConfigureDisplayMirrorOfDisplay(config, displayId, kCGNullDirectDisplay);
  }
  CGCompleteDisplayConfiguration(config, kCGConfigureForAppOnly);

  boolean_t isMirror = CGDisplayIsInMirrorSet(_display.displayID);
  NSLog(@"Virtual Display is in mirror set: %d", isMirror);
  CGBeginDisplayConfiguration(&config);
  if (useMirror) {
    if (isMirror == 0) {
      NSLog(@"Enable Virtual Display mirror mode");
      // set mirror mode
      CGError err = CGConfigureDisplayMirrorOfDisplay(
          config, _display.displayID, mainDisplay);
      if (err != kCGErrorSuccess) {
        NSLog(@"Failed to enable mirror mode: %d", err);
      }
    }
  } else {
    if (isMirror == 1) {
      NSLog(@"Disable Virtual Display mirror mode");
      // if already in mirror mode, disable mirror mode
      CGError err = CGConfigureDisplayMirrorOfDisplay(
          config, _display.displayID, kCGNullDirectDisplay);
      if (err != kCGErrorSuccess) {
        NSLog(@"Failed to disable mirror mode: %d", err);
      }
    }
  }
  CGCompleteDisplayConfiguration(config, kCGConfigureForAppOnly);
  // postprocess end

  return CreateDisplayObject(width, height);
}

bool VDisplay::DestroyVirtualDisplay() {
  if (_display) {
    [_descriptor release];
    _descriptor = nil;
    [_settings release];
    _settings = nil;
    [_display release];
    _display = nil;
    return true;
  } else {
    return false;
  }
}
