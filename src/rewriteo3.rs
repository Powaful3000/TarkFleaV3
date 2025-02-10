// rewrite.rs
//
// A complete rewrite and improvement of the Escape From Tarkov Flea Market Bot POC.
// This file implements a singleton frame recorder that continuously captures screenshots
// (aiming for 60fps) from the target window and stores them in a small ring buffer.
// It also includes captcha detection functionality (using template matching) to scan
// the most recent frame for known patterns. All core functionality is implemented here
// and does not rely on your previous implementations.
//
// Crates used (as before): 
// - tokio, lazy_static, windows-capture, image, png, template-matching, etc.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use lazy_static::lazy_static;
use tokio::sync::Mutex;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};
use image::{DynamicImage, RgbaImage};
use template_matching::{TemplateMatcher, Image as TMImage, MatchTemplateMethod, find_extremes};

/// FrameData holds the raw pixel data and metadata for a captured frame.
#[derive(Clone)]
pub struct FrameData {
    /// The raw pixel data, represented as a vector of bytes in RGBA format.
    pub pixels: Arc<Vec<u8>>,
    /// The width of the captured frame in pixels.
    pub width: u32,
    /// The height of the captured frame in pixels.
    pub height: u32,
    /// The stride of the captured frame (bytes per row).
    pub stride: u32,
    /// The timestamp when the frame was captured.
    pub timestamp: Instant,
}

/// FrameRecorder is a singleton that maintains a ring buffer of the most recent frames.
pub struct FrameRecorder {
    /// The buffer storing the captured frames. Uses a `Mutex` for thread safety and a `VecDeque` for efficient FIFO.
    buffer: Mutex<VecDeque<FrameData>>,
    /// The maximum number of frames to store in the buffer.
    max_frames: usize,
    /// Sender for sending new frames to the recorder.
    tx: tokio::sync::mpsc::Sender<FrameData>,
}

impl FrameRecorder {
    /// Creates a new FrameRecorder with the specified ring buffer capacity.
    ///
    /// # Arguments
    ///
    /// * `max_frames`: The maximum number of frames to store.
    pub fn new(max_frames: usize) -> Arc<Self> {
        // Create a channel that will receive new frames from the capture callback.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FrameData>(64);
        let recorder = Arc::new(FrameRecorder {
            buffer: Mutex::new(VecDeque::with_capacity(max_frames)),
            max_frames,
            tx,
        });

        // Spawn a background task to drain the channel and update the ring buffer.
        let recorder_clone = Arc::clone(&recorder);
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let mut buffer = recorder_clone.buffer.lock().await;
                if buffer.len() >= recorder_clone.max_frames {
                    buffer.pop_front();
                }
                buffer.push_back(frame);
            }
        });

        recorder
    }

    /// Returns a clone of the sender so that frames can be pushed into the recorder.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<FrameData> {
        self.tx.clone()
    }

    /// Retrieves the most recent frame from the ring buffer.
    pub async fn latest_frame(&self) -> Option<FrameData> {
        let buffer = self.buffer.lock().await;
        buffer.back().cloned()
    }
}

// Create a global singleton recorder (with capacity for 64 frames).
lazy_static! {
    /// `RECORDER` is a global, lazily-initialized `FrameRecorder` instance.
    /// It's wrapped in an `Arc` to allow for shared ownership across threads.
    static ref RECORDER: Arc<FrameRecorder> = FrameRecorder::new(64);
}

/// CaptureHandler implements the windows-capture trait to receive new frames from the target window.
struct CaptureHandler {
    /// Sender for sending captured frames to the FrameRecorder.
    sender: tokio::sync::mpsc::Sender<FrameData>,
    /// Timestamp of when the capture started.
    start_time: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = String;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    /// Creates a new CaptureHandler.
    ///
    /// # Arguments
    ///
    /// * `_`: Placeholder for capture context (not used here).
    fn new(_: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            // Get the sender from our singleton recorder.
            sender: RECORDER.sender(),
            start_time: Instant::now(),
        })
    }

    /// Called when a new frame arrives from the capture session.
    ///
    /// # Arguments
    ///
    /// * `frame`: A mutable reference to the captured frame.
    /// * `_capture_control`:  Provides internal capture control (not used here).
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let stride = width * 4; // Bytes per row (RGBA = 4 bytes per pixel).

        // Attempt to get the frame's buffer.
        if let Ok(mut buffer) = frame.buffer() {
            // Create a vector to store the pixel data.
            let mut pixels = vec![0u8; (stride * height) as usize];
            // Copy the frame data into our pixel vector.
            buffer.copy_to_slice(&mut pixels)?;

            // Create a FrameData struct to hold the frame's information.
            let frame_data = FrameData {
                pixels: Arc::new(pixels),
                width,
                height,
                stride,
                timestamp: Instant::now(),
            };

            // Use blocking_send because this callback is synchronous.
            if let Err(e) = self.sender.blocking_send(frame_data) {
                eprintln!("Failed to send frame data: {}", e);
            }
        } else {
            eprintln!("Failed to get frame buffer");
        }

        Ok(())
    }

    /// Called when the capture session is closed.
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}

/// The captcha module contains functions for detecting captcha patterns
/// using template matching.
mod captcha {
    use super::{FrameData, find_extremes, MatchTemplateMethod, TemplateMatcher, TMImage};
    use image::{DynamicImage, RgbaImage};
    use lazy_static::lazy_static;

    // Load the captcha template once (using an image file embedded at compile time).
    lazy_static! {
        /// `CAPTCHA_TEMPLATE` is a lazily-initialized static variable that holds the loaded captcha template image.
        /// It stores the pixel data as `Vec<f32>`, along with the width and height.
        static ref CAPTCHA_TEMPLATE: (Vec<f32>, u32, u32) = {
            let template_image = image::load_from_memory(include_bytes!("captcha-template.png"))
                .expect("Failed to load captcha template")
                .to_luma32f();
            let data = template_image.as_raw().clone();
            let width = template_image.width();
            let height = template_image.height();
            (data, width, height)
        };
    }

    /// Scans the provided frame for a captcha using template matching.
    ///
    /// Returns Ok(true) if a captcha match is detected (i.e. the match score is below
    /// the chosen threshold), Ok(false) if no match is found, or an error.
    pub fn detect_captcha(frame: &FrameData) -> Result<bool, Box<dyn std::error::Error>> {
        // Convert the raw frame data into an image.
        let rgba_image = RgbaImage::from_raw(frame.width, frame.height, (*frame.pixels).clone())
            .ok_or("Invalid frame dimensions")?;
        let dynamic_image = DynamicImage::ImageRgba8(rgba_image);
        let frame_luma = dynamic_image.to_luma32f();

        // Prepare images for template matching.
        let frame_img = TMImage::new(frame_luma.as_raw().clone(), frame_luma.width(), frame_luma.height());
        let (template_data, template_width, template_height) = &*CAPTCHA_TEMPLATE;
        let template_img = TMImage::new(template_data.clone(), *template_width, *template_height);

        let mut matcher = TemplateMatcher::new();
        matcher.match_template(frame_img, template_img, MatchTemplateMethod::SumOfSquaredDifferences);
        let result = matcher
        .wait_for_result()
        .ok_or("Template matching failed to produce a result")?;
            let extremes = find_extremes(&result);
        let captcha_threshold = 25.0;

        if extremes.min_value < captcha_threshold {
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// The main function initializes the capture and processing loop.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Initializing Escape From Tarkov Flea Market Bot...");

    // Locate the target window.
    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?;
    println!("Found target window: EscapeFromTarkov");

    // Configure the capture settings.
    let settings = Settings::new(
        target_window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        ColorFormat::Rgba8,
        String::new(),
    );

    // Start the capture session in a dedicated blocking thread.
    println!("Starting capture (targeting 60fps)...");
    let _capture_handle = tokio::task::spawn_blocking(move || {
        println!("Capture thread started.");
        // This call blocks while capturing; it will call our CaptureHandler callbacks.
        if let Err(e) = CaptureHandler::start(settings) {
            eprintln!("Capture encountered an error: {:?}", e);
        }
    });

    // Main processing loop: retrieve the most recent frame and scan for captcha.
    loop {
        if let Some(frame) = RECORDER.latest_frame().await {
            println!(
                "Latest frame: {}x{} captured at {:?}",
                frame.width, frame.height, frame.timestamp
            );

            // Run captcha detection on the latest frame.
            match captcha::detect_captcha(&frame) {
                Ok(true) => println!("Captcha detected on screen!"),
                Ok(false) => println!("No captcha detected."),
                Err(e) => eprintln!("Error during captcha detection: {}", e),
            }
        } else {
            println!("No frame available yet.");
        }
        // Sleep roughly 16ms to approach 60fps processing (adjust as necessary).
        tokio::time::sleep(tokio::time::Duration::from_millis(16)).await;
    }

    // Note: Because the main loop runs indefinitely, the capture thread is never awaited.
    // When integrating further functionality, you may wish to gracefully shut down.
}
