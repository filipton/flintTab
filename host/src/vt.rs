//! In-process low-latency H.264 encoder on VideoToolbox.
//!
//! Frames come straight from ScreenCaptureKit as IOSurface-backed NV12 pixel buffers
//! (no copy, no pipe). The session is configured for real-time use: hardware encoder,
//! low-latency rate control, Constrained High profile, no B-frames, and every frame is
//! flushed out of the encoder immediately (`CompleteFrames`), so one frame in = one
//! access unit out with no encoder-side queueing.

use anyhow::{Result, bail};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use std::{ffi::c_void, ptr};

type OSStatus = i32;
type Session = *mut c_void;
type SampleBuffer = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}
const TIME_VALID: u32 = 1;
const TIME_INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, SampleBuffer);

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxFrameDelayCount: CFStringRef;
    static kVTCompressionPropertyKey_ColorPrimaries: CFStringRef;
    static kVTCompressionPropertyKey_TransferFunction: CFStringRef;
    static kVTCompressionPropertyKey_YCbCrMatrix: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedHigh_AutoLevel: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_spec: *const c_void,
        source_attrs: *const c_void,
        compressed_allocator: *const c_void,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut Session,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: Session, key: CFStringRef, value: *const c_void) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: Session) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: Session,
        image: *mut c_void,
        pts: CMTime,
        duration: CMTime,
        frame_props: *const c_void,
        frame_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: Session, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: Session);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVImageBufferColorPrimaries_ITU_R_709_2: CFStringRef;
    static kCVImageBufferTransferFunction_ITU_R_709_2: CFStringRef;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: CFStringRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;

    fn CMSampleBufferGetDataBuffer(sb: SampleBuffer) -> *mut c_void;
    fn CMSampleBufferGetFormatDescription(sb: SampleBuffer) -> *mut c_void;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: SampleBuffer, create: u8) -> *const c_void;
    fn CMBlockBufferGetDataLength(bb: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(bb: *mut c_void, offset: usize, len: usize, dst: *mut u8) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        fmt: *mut c_void,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetValueAtIndex(a: *const c_void, i: isize) -> *const c_void;
    fn CFDictionaryGetValue(d: *const c_void, key: *const c_void) -> *const c_void;
}

const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");

type Sink = Box<dyn FnMut(Vec<u8>, u64) + Send>;

pub struct VtEncoder {
    session: Session,
    sink: *mut Sink,
    force_key: CFDictionary<CFString, CFType>,
}

// The VT session is thread-safe; `sink` is only touched by VT's callback and by Drop
// after the session has been invalidated.
unsafe impl Send for VtEncoder {}

fn cf_key(k: CFStringRef) -> CFString {
    unsafe { CFString::wrap_under_get_rule(k) }
}

impl VtEncoder {
    /// `on_au(annex_b_access_unit, pts_us)` is called once per encoded frame.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_mbps: u32,
        on_au: impl FnMut(Vec<u8>, u64) + Send + 'static,
    ) -> Result<Self> {
        let sink: *mut Sink = Box::into_raw(Box::new(Box::new(on_au) as Sink));
        let t = CFBoolean::true_value();
        let spec = unsafe {
            CFDictionary::from_CFType_pairs(&[
                (cf_key(kVTVideoEncoderSpecification_EnableLowLatencyRateControl), t.as_CFType()),
                (cf_key(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder), t.as_CFType()),
            ])
        };

        let mut session: Session = ptr::null_mut();
        let st = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                CODEC_H264,
                spec.as_concrete_TypeRef() as *const c_void,
                ptr::null(),
                ptr::null(),
                output_callback,
                sink as *mut c_void,
                &mut session,
            )
        };
        if st != 0 || session.is_null() {
            unsafe { drop(Box::from_raw(sink)) };
            bail!("VTCompressionSessionCreate failed ({st}); hardware low-latency H.264 unavailable");
        }

        let bps = bitrate_mbps as i64 * 1_000_000;
        let set = |key: CFStringRef, value: &CFType| {
            let st = unsafe { VTSessionSetProperty(session, key, value.as_concrete_TypeRef() as *const c_void) };
            if st != 0 {
                eprintln!("warning: VT property {} rejected ({st})", cf_key(key));
            }
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, &t.as_CFType());
            set(
                kVTCompressionPropertyKey_ProfileLevel,
                &CFString::wrap_under_get_rule(kVTProfileLevel_H264_ConstrainedHigh_AutoLevel).as_CFType(),
            );
            set(kVTCompressionPropertyKey_AllowFrameReordering, &CFBoolean::false_value().as_CFType());
            set(kVTCompressionPropertyKey_AverageBitRate, &CFNumber::from(bps).as_CFType());
            set(
                kVTCompressionPropertyKey_DataRateLimits,
                &CFArray::from_CFTypes(&[CFNumber::from(bps * 3 / 2 / 8), CFNumber::from(1i64)]).as_CFType(),
            );
            set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::from(fps as i32).as_CFType());
            // TCP is lossless, so IDR frames are only needed for the very first frame;
            // a long GOP avoids periodic bitrate spikes.
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &CFNumber::from(fps as i32 * 20).as_CFType());
            set(kVTCompressionPropertyKey_MaxFrameDelayCount, &CFNumber::from(0i32).as_CFType());
            // Tag the stream as BT.709 (what ScreenCaptureKit produces) so the SPS carries
            // colour info and the tablet does not guess BT.601 and shift the colours.
            set(kVTCompressionPropertyKey_ColorPrimaries, &cf_key(kCVImageBufferColorPrimaries_ITU_R_709_2).as_CFType());
            set(kVTCompressionPropertyKey_TransferFunction, &cf_key(kCVImageBufferTransferFunction_ITU_R_709_2).as_CFType());
            set(kVTCompressionPropertyKey_YCbCrMatrix, &cf_key(kCVImageBufferYCbCrMatrix_ITU_R_709_2).as_CFType());
            VTCompressionSessionPrepareToEncodeFrames(session);
        }
        let force_key = unsafe {
            CFDictionary::from_CFType_pairs(&[(cf_key(kVTEncodeFrameOptionKey_ForceKeyFrame), t.as_CFType())])
        };
        Ok(Self { session, sink, force_key })
    }

    /// Encodes one IOSurface-backed CVPixelBuffer and blocks until its access unit was
    /// delivered to the sink (the encoder may also drop it to hold the bitrate).
    pub fn encode(&self, pixel_buffer: *mut c_void, pts_us: u64, keyframe: bool) -> bool {
        let pts = CMTime { value: pts_us as i64, timescale: 1_000_000, flags: TIME_VALID, epoch: 0 };
        unsafe {
            let st = VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                pts,
                TIME_INVALID,
                if keyframe { self.force_key.as_concrete_TypeRef() as *const c_void } else { ptr::null() },
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if st != 0 {
                return false;
            }
            VTCompressionSessionCompleteFrames(self.session, TIME_INVALID);
        }
        true
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, TIME_INVALID);
            VTCompressionSessionInvalidate(self.session);
            drop(Box::from_raw(self.sink));
        }
    }
}

extern "C" fn output_callback(refcon: *mut c_void, _frame: *mut c_void, status: OSStatus, _flags: u32, sb: SampleBuffer) {
    if status != 0 || sb.is_null() {
        return;
    }
    unsafe {
        let sink = &mut *(refcon as *mut Sink);
        let mut out: Vec<u8> = Vec::new();

        // Keyframe = no NotSync attachment. Prepend SPS/PPS so the decoder can (re)start.
        let atts = CMSampleBufferGetSampleAttachmentsArray(sb, 0);
        let key = atts.is_null()
            || CFDictionaryGetValue(CFArrayGetValueAtIndex(atts, 0), kCMSampleAttachmentKey_NotSync as *const c_void)
                .is_null();
        if key {
            let fmt = CMSampleBufferGetFormatDescription(sb);
            let mut count = 0usize;
            let mut nal_len = 0i32;
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, 0, ptr::null_mut(), ptr::null_mut(), &mut count, &mut nal_len);
            for i in 0..count {
                let mut p: *const u8 = ptr::null();
                let mut n = 0usize;
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fmt, i, &mut p, &mut n, ptr::null_mut(), ptr::null_mut()) == 0 {
                    let ps = std::slice::from_raw_parts(p, n);
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    match crate::sps::add_low_latency_vui(ps) {
                        Some(sps) => out.extend_from_slice(&sps),
                        None => out.extend_from_slice(ps), // PPS, or an SPS we could not parse
                    }
                }
            }
        }

        let bb = CMSampleBufferGetDataBuffer(sb);
        let len = CMBlockBufferGetDataLength(bb);
        let mut avcc = vec![0u8; len];
        if CMBlockBufferCopyDataBytes(bb, 0, len, avcc.as_mut_ptr()) != 0 {
            return;
        }
        avcc_to_annexb(&avcc, &mut out);

        // pts is not needed on the wire; the receiver renders immediately.
        sink(out, 0);
    }
}

/// Rewrites 4-byte length-prefixed NAL units as Annex-B start-code-prefixed ones.
pub fn avcc_to_annexb(avcc: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i + 4 <= avcc.len() {
        let n = u32::from_be_bytes(avcc[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + n > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[i..i + n]);
        i += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_lengths_to_start_codes() {
        let avcc = [0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        avcc_to_annexb(&avcc, &mut out);
        assert_eq!(out, vec![0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41]);
    }
}
