//! Video playback through AVFoundation.
//!
//! The first attempt at this decoded clips to JPEG sequences with ffmpeg and
//! swapped them on a timer. That is a hack, and it does not survive a real
//! render: no audio, no clock, and a 21 minute file cost seconds of decoding
//! and hundreds of megabytes before anything appeared.
//!
//! This hands the work to the platform. `AVPlayer` decodes in hardware, plays
//! the audio and owns the clock; `AVPlayerItemVideoOutput` yields each frame as
//! a `CVPixelBuffer`, which gpui's `surface()` element binds directly as Metal
//! textures.
//!
//! The pixel format is not a preference. gpui's Metal renderer asserts on it:
//!
//! ```ignore
//! assert_eq!(surface.image_buffer.get_pixel_format(),
//!            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
//! ```
//!
//! so the output is configured to produce exactly that, IOSurface-backed. The
//! decoder writes a buffer, Metal samples the same memory, and nothing is
//! copied or converted on the way.

#![allow(unexpected_cfgs)]

use anyhow::{Result, bail};
use core_video::pixel_buffer::CVPixelBuffer;
use std::path::Path;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use core_foundation::base::TCFType;
    use core_video::pixel_buffer::CVPixelBufferRef;
    use objc::{
        class, msg_send,
        runtime::{NO, Object, YES},
        sel, sel_impl,
    };
    use std::ffi::c_void;

    type Id = *mut Object;

    /// The one format gpui's renderer accepts: bi-planar YUV 4:2:0, full range.
    const PIXEL_FORMAT: i32 = 0x34323066; // '420f'

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeGetSeconds(time: CMTime) -> f64;
        fn CMTimeMakeWithSeconds(seconds: f64, preferred_timescale: i32) -> CMTime;
    }

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        static kCVPixelBufferPixelFormatTypeKey: *const c_void;
        static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    }

    // Linked so the classes below resolve at runtime.
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after: u8) -> i32;
        static kCFRunLoopDefaultMode: *const c_void;
    }

    /// Give AVFoundation's run loop a slice of time.
    ///
    /// Asset loading and status changes are delivered on the run loop, so a
    /// process without one — a test harness, a CLI — never sees an item become
    /// ready. The app has AppKit's, and does not need this.
    pub fn pump(seconds: f64) {
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0);
        }
    }

    /// A clip open in AVFoundation, decoding into buffers gpui can draw.
    pub struct Player {
        player: Id,
        item: Id,
        output: Id,
        /// Cached because the track only reports it once the asset has loaded.
        fps: f64,
    }

    impl Player {
        pub fn open(path: &Path) -> Result<Self> {
            unsafe {
                let path = path.to_string_lossy();
                let path: Id = msg_send![class!(NSString), stringWithUTF8String:
                    std::ffi::CString::new(path.as_ref())?.as_ptr()];
                let url: Id = msg_send![class!(NSURL), fileURLWithPath: path];
                if url.is_null() {
                    bail!("could not open {path:?}");
                }

                let item: Id = msg_send![class!(AVPlayerItem), playerItemWithURL: url];
                if item.is_null() {
                    bail!("AVFoundation could not read the clip");
                }

                // The attributes that make the decoder write what gpui can bind:
                // the exact bi-planar format, backed by an IOSurface so Metal
                // can sample the decoder's own memory.
                let format: Id = msg_send![class!(NSNumber), numberWithInt: PIXEL_FORMAT];
                let surface: Id = msg_send![class!(NSDictionary), dictionary];
                let keys: [*const c_void; 2] = [
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ];
                let values: [Id; 2] = [format, surface];
                let attributes: Id = msg_send![class!(NSDictionary),
                    dictionaryWithObjects: values.as_ptr()
                    forKeys: keys.as_ptr()
                    count: 2usize];

                let output: Id = msg_send![class!(AVPlayerItemVideoOutput), alloc];
                let output: Id = msg_send![output, initWithPixelBufferAttributes: attributes];
                if output.is_null() {
                    bail!("could not create a video output");
                }
                let _: () = msg_send![item, addOutput: output];

                let player: Id = msg_send![class!(AVPlayer), playerWithPlayerItem: item];
                if player.is_null() {
                    bail!("could not create a player");
                }
                // Frames are pulled deliberately, so the player must not stall
                // waiting for a display link we do not own.
                let _: () = msg_send![player, setAutomaticallyWaitsToMinimizeStalling: NO];

                let player = Self {
                    player: msg_send![player, retain],
                    item: msg_send![item, retain],
                    output: msg_send![output, retain],
                    fps: 0.,
                };
                Ok(player)
            }
        }

        /// Seconds of clip, or zero until the item has loaded enough to say.
        pub fn duration(&self) -> f64 {
            unsafe {
                let time: CMTime = msg_send![self.item, duration];
                let seconds = CMTimeGetSeconds(time);
                if seconds.is_finite() && seconds > 0. {
                    seconds
                } else {
                    0.
                }
            }
        }

        /// Frames per second, read from the video track once it is loaded. This
        /// is what turns a comment's time into the frame a composition names.
        pub fn fps(&mut self) -> f64 {
            if self.fps > 0. {
                return self.fps;
            }
            unsafe {
                let asset: Id = msg_send![self.item, asset];
                if asset.is_null() {
                    return 0.;
                }
                let media: Id = msg_send![class!(NSString), stringWithUTF8String:
                    c"vide".as_ptr()];
                let tracks: Id = msg_send![asset, tracksWithMediaType: media];
                if tracks.is_null() {
                    return 0.;
                }
                let count: usize = msg_send![tracks, count];
                if count == 0 {
                    return 0.;
                }
                let track: Id = msg_send![tracks, objectAtIndex: 0usize];
                let rate: f32 = msg_send![track, nominalFrameRate];
                if rate > 0. {
                    self.fps = f64::from(rate);
                }
                self.fps
            }
        }

        pub fn current_time(&self) -> f64 {
            unsafe {
                let time: CMTime = msg_send![self.player, currentTime];
                let seconds = CMTimeGetSeconds(time);
                if seconds.is_finite() {
                    seconds.max(0.)
                } else {
                    0.
                }
            }
        }

        pub fn is_playing(&self) -> bool {
            unsafe {
                let rate: f32 = msg_send![self.player, rate];
                rate != 0.
            }
        }

        pub fn play(&self) {
            unsafe {
                let _: () = msg_send![self.player, play];
            }
        }

        pub fn pause(&self) {
            unsafe {
                let _: () = msg_send![self.player, pause];
            }
        }

        /// Seek precisely — the default tolerance snaps to keyframes, which is
        /// no use when the comment records a frame number.
        pub fn seek(&self, seconds: f64) {
            unsafe {
                let time = CMTimeMakeWithSeconds(seconds.max(0.), 600);
                let zero = CMTime {
                    value: 0,
                    timescale: 600,
                    flags: 1,
                    epoch: 0,
                };
                let _: () = msg_send![self.player, seekToTime: time
                    toleranceBefore: zero
                    toleranceAfter: zero];
            }
        }

        /// Whether the clip has run out, so playback can stop at the end.
        pub fn is_finished(&self) -> bool {
            let duration = self.duration();
            duration > 0. && self.current_time() >= duration - 0.05
        }

        /// The frame for right now, if the output has a new one.
        ///
        /// Returning `None` is the normal case between frames — the caller
        /// keeps drawing whatever it last received.
        pub fn frame(&self) -> Option<CVPixelBuffer> {
            unsafe {
                let time: CMTime = msg_send![self.item, currentTime];
                let has_new: bool = {
                    let value: i8 = msg_send![self.output, hasNewPixelBufferForItemTime: time];
                    value != NO as i8
                };
                if !has_new {
                    return None;
                }

                let buffer: CVPixelBufferRef = msg_send![self.output,
                    copyPixelBufferForItemTime: time
                    itemTimeForDisplay: std::ptr::null_mut::<CMTime>()];
                if buffer.is_null() {
                    return None;
                }
                // copyPixelBuffer follows the create rule, so ownership
                // transfers here and the wrapper releases it.
                Some(CVPixelBuffer::wrap_under_create_rule(buffer))
            }
        }

        /// True once the item can report its duration and hand over frames.
        pub fn is_ready(&self) -> bool {
            unsafe {
                let status: i64 = msg_send![self.item, status];
                // AVPlayerItemStatusReadyToPlay
                status == 1
            }
        }

        pub fn failed(&self) -> bool {
            unsafe {
                let status: i64 = msg_send![self.item, status];
                // AVPlayerItemStatusFailed
                status == 2
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            unsafe {
                let _: () = msg_send![self.player, pause];
                let _: () = msg_send![self.item, removeOutput: self.output];
                let _: () = msg_send![self.output, release];
                let _: () = msg_send![self.item, release];
                let _: () = msg_send![self.player, release];
            }
        }
    }

    // AVPlayer is used only from the main thread, where gpui renders.
    const _: fn() = || {
        let _ = YES;
    };
}

#[cfg(target_os = "macos")]
pub use platform::{Player, pump};

/// Outside macOS there is no run loop to give time to.
#[cfg(not(target_os = "macos"))]
pub fn pump(_: f64) {}

#[cfg(not(target_os = "macos"))]
pub struct Player;

#[cfg(not(target_os = "macos"))]
impl Player {
    pub fn open(_: &Path) -> Result<Self> {
        bail!("video playback is only implemented on macOS")
    }
    pub fn duration(&self) -> f64 {
        0.
    }
    pub fn fps(&mut self) -> f64 {
        0.
    }
    pub fn current_time(&self) -> f64 {
        0.
    }
    pub fn is_playing(&self) -> bool {
        false
    }
    pub fn play(&self) {}
    pub fn pause(&self) {}
    pub fn seek(&self, _: f64) {}
    pub fn is_finished(&self) -> bool {
        true
    }
    pub fn frame(&self) -> Option<CVPixelBuffer> {
        None
    }
    pub fn is_ready(&self) -> bool {
        false
    }
    pub fn failed(&self) -> bool {
        true
    }
}
