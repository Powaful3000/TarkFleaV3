// src/rewrite.rs
use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::{DynamicImage, GenericImageView, ImageBuffer, Luma};
use lazy_static::lazy_static;
use template_matching::{find_extremes, Image as TemplateImage, MatchTemplateMethod, TemplateMatcher};
use tokio::sync::{Mutex, MutexGuard, mpsc};
use tokio::time::interval;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};

// FrameData struct to hold frame information
/// `FrameData` holds the raw pixel data of a captured frame, along with its dimensions and capture timestamp.
#[derive(Clone)]
pub struct FrameData {
    /// The raw pixel data, represented as a vector of bytes in RGBA format.
    pixels: Arc<Vec<u8>>,
    /// The width of the captured frame in pixels.
    width: u32,
    /// The height of the captured frame in pixels.
    height: u32,
    /// The stride of the captured frame (bytes per row).
    stride: u32,
    /// The timestamp when the frame was captured.
    timestamp: Instant,
}

// FrameRecorder struct to manage frame capture and buffer
/// `FrameRecorder` manages the capturing and buffering of frames. It stores a limited number of recent frames.
pub struct FrameRecorder {
    /// The buffer storing the captured frames.  Uses a `VecDeque` for efficient FIFO behavior.
    buffer: Arc<Mutex<VecDeque<FrameData>>>,
    /// The maximum number of frames to store in the buffer.
    max_frames: usize,
    /// A sender for transmitting `FrameData` to the recorder.
    frame_sender: mpsc::Sender<FrameData>,
}

impl FrameRecorder {
    /// Creates a new `FrameRecorder` with the specified buffer capacity.
    ///
    /// # Arguments
    ///
    /// * `max_frames`: The maximum number of frames to store in the buffer.
    fn new(max_frames: usize) -> Self {
        let (frame_sender, frame_receiver) = mpsc::channel(32); // Channel for sending frames to the recorder
        let buffer: Arc<Mutex<VecDeque<FrameData>>> = Arc::new(Mutex::new(VecDeque::with_capacity(max_frames)));
        let buffer_clone = buffer.clone();

        // Spawn a tokio task to manage the frame buffer
        tokio::spawn(async move {
            FrameRecorder::frame_buffer_task(buffer_clone, frame_receiver, max_frames).await;
        });

        Self {
            buffer,
            max_frames,
            frame_sender,
        }
    }

    /// Asynchronously manages the frame buffer.  Receives frames from the `frame_receiver` and adds them to the buffer.
    /// If the buffer is full, the oldest frame is removed.
    ///
    /// # Arguments
    ///
    /// * `buffer`: An `Arc<Mutex<VecDeque<FrameData>>>` representing the frame buffer.
    /// * `frame_receiver`: An `mpsc::Receiver<FrameData>` for receiving new frames.
    /// * `max_frames`: The maximum number of frames the buffer can hold.
    async fn frame_buffer_task(
        buffer: Arc<Mutex<VecDeque<FrameData>>>,
        mut frame_receiver: mpsc::Receiver<FrameData>,
        max_frames: usize,
    ) {
        while let Some(frame) = frame_receiver.recv().await {
            let mut guard: MutexGuard<'_, VecDeque<FrameData>> = buffer.lock().await;
            if guard.len() >= max_frames {
                guard.pop_front(); // Remove oldest frame if buffer is full
            }
            guard.push_back(frame); // Add the latest frame
        }
        println!("Frame receiver channel closed, frame buffer task завершено."); // Log when channel is closed (if needed for debugging)
    }


    // Get the latest frame from the buffer
    /// Retrieves the most recently captured frame from the buffer.
    ///
    /// Returns `None` if the buffer is empty, otherwise returns `Some(FrameData)`.
    pub async fn get_latest_frame(&self) -> Option<FrameData> {
        let buffer_lock = self.buffer.lock().await;
        buffer_lock.back().cloned()
    }

    // Get the sender to send frames to the recorder
    /// Returns a clone of the `mpsc::Sender<FrameData>` that can be used to send new frames to the recorder.
    pub fn get_frame_sender(&self) -> mpsc::Sender<FrameData> {
        self.frame_sender.clone()
    }
}

// Lazy static initialization of the global FrameRecorder singleton
lazy_static! {
    /// A global, lazily-initialized `FrameRecorder` instance.  This provides a single point of access to the captured frames.
    static ref GLOBAL_RECORDER: FrameRecorder = FrameRecorder::new(5); // Example: buffer of 5 frames
}

// Function to access the global FrameRecorder instance
/// Returns a reference to the global `FrameRecorder` instance.
pub fn get_global_recorder() -> &'static FrameRecorder {
    &GLOBAL_RECORDER
}


/// `CaptureSessionHandler` implements the `GraphicsCaptureApiHandler` trait to handle captured frames.
struct CaptureSessionHandler {
    /// Sender for sending captured frames to the `FrameRecorder`.
    frame_tx: mpsc::Sender<FrameData>,
}

impl GraphicsCaptureApiHandler for CaptureSessionHandler {
    type Flags = String;
    type Error = Box<dyn Error + Send + Sync>;

    /// Creates a new `CaptureSessionHandler`.
    ///
    /// # Arguments
    ///
    /// * `_flags`:  Flags associated with the capture session (not used in this implementation).
    fn new(_flags: Self::Flags) -> Result<Self, Self::Error> {
        // Get the global recorder's frame sender to send frames to the buffer.
        let frame_tx = get_global_recorder().get_frame_sender();
        Ok(Self { frame_tx })
    }

    /// Called when a new frame is available from the capture session.
    /// This function extracts the raw pixel data from the `Frame`, creates a `FrameData` object,
    /// and sends it to the `FrameRecorder` via the `frame_tx` channel.
    ///
    /// # Arguments
    ///
    /// * `frame`: A mutable reference to the captured `Frame`.
    /// * `_capture_control`: An `InternalCaptureControl` object (not used in this implementation).
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let stride = width * 4;

        if let Ok(mut frame_buffer) = frame.buffer() {
            let mut pixels = vec![0u8; (stride * height) as usize];
            frame_buffer.copy_to_slice(pixels.as_mut_slice())?;

            let frame_data = FrameData {
                pixels: Arc::new(pixels),
                width,
                height,
                stride,
                timestamp: Instant::now(),
            };

            // Send the frame data to the recorder.  If the channel is full, this will log an error.
            if let Err(e) = self.frame_tx.blocking_send(frame_data) {
                eprintln!("Error sending frame to recorder channel: {}", e);
                // Consider handling backpressure or logging more appropriately if needed.
            }

        } else {
            println!("Failed to get frame buffer");
        }
        Ok(())
    }

    /// Called when the capture session is closed.
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}


/// Main function for the example application.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Starting Escape From Tarkov Bot POC Rewrite...");

    // Find the target window by its name.
    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?;
    println!("Found target window: {:?}", target_window);

    // Create capture settings.
    let settings = Settings::new(
        target_window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        ColorFormat::Rgba8,
        String::new(), // Flags can be empty string
    );

    println!("Starting capture session...");
    // Start the capture session in a blocking task.
    let capture_result = tokio::task::spawn_blocking(move || {
        CaptureSessionHandler::start(settings)
    }).await;

    // Check the result of starting the capture session.
    match capture_result {
        Ok(Ok(_)) => println!("Capture session started successfully in background task."),
        Ok(Err(e)) => eprintln!("Error starting capture session: {}", e),
        Err(e) => eprintln!("Error joining capture task: {}", e), // Error from spawn_blocking itself
    }


    // Load template image for template matching
    let template_dyn = image::load_from_memory(include_bytes!("captcha-template.png"))
        .unwrap()
        .to_luma32f();
    let template_data = template_dyn.as_raw().to_vec();
    let template_width = template_dyn.width();
    let template_height = template_dyn.height();
    println!("Template loaded: {}x{}", template_width, template_height);


    let mut matcher = TemplateMatcher::new();

    println!("Entering main loop to process frames...");
    let mut frame_interval = interval(Duration::from_millis(1000/60)); // Target 60 FPS, though actual FPS depends on capture speed and processing

    loop {
        frame_interval.tick().await; // Wait for the next tick to control frame processing rate

        if let Some(frame_data) = get_global_recorder().get_latest_frame().await {
            println!(
                "Got frame: {}x{}, stride: {}, {} bytes",
                frame_data.width,
                frame_data.height,
                frame_data.stride,
                frame_data.pixels.len()
            );

            // Save frame for debugging
            if false { // Conditional frame saving, set to true to enable for debug
                save_frame_to_png(&frame_data, "latest_frame.png").await?;
            }


            // Convert captured frame to grayscale and prepare for template matching
            let frame_luma_image = convert_frame_to_luma32f(&frame_data)?;
            let frame_image = TemplateImage::new(
                frame_luma_image.as_raw().to_vec(),
                frame_luma_image.width(),
                frame_luma_image.height(),
            );
            let template_image = TemplateImage::new(
                template_data.clone(),
                template_width,
                template_height,
            );

            // Perform template matching
            matcher.match_template(
                frame_image,
                template_image,
                MatchTemplateMethod::SumOfSquaredDifferences,
            );
            let result = matcher.wait_for_result().unwrap();
            let extremes = find_extremes(&result);
            println!(
                "Template Match Result: Min Value: {}, Location: {:?}",
                extremes.min_value, extremes.min_value_location
            );

            let captcha_threshold = 25.0; // Example threshold
            if extremes.min_value < captcha_threshold {
                println!("Captcha match found!! (Value: {})", extremes.min_value);
            } else {
                println!("Captcha not matched (Value: {})", extremes.min_value);
            }


        } else {
            println!("No frame available yet from recorder.");
        }
    }
}


/// Saves a `FrameData` object to a PNG file.
///
/// # Arguments
///
/// * `frame_data`: A reference to the `FrameData` to save.
/// * `filename`: The name of the file to save the image to.
async fn save_frame_to_png(frame_data: &FrameData, filename: &str) -> Result<(), Box<dyn Error>> {
    let file = File::create(filename)?;
    let w = BufWriter::new(file);
    let mut encoder = png::Encoder::new(w, frame_data.width, frame_data.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(frame_data.pixels.as_ref())?;
    println!("Saved frame to {}", filename);
    Ok(())
}


/// Converts a `FrameData` object to a `Luma32F` image buffer.
///
/// # Arguments
///
/// * `frame_data`: A reference to the `FrameData` to convert.
fn convert_frame_to_luma32f(frame_data: &FrameData) -> Result<ImageBuffer<Luma<f32>, Vec<f32>>, Box<dyn Error>> {
    let rgba_image = ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        frame_data.width,
        frame_data.height,
        frame_data.pixels.as_ref().to_vec(),
    ).ok_or("Failed to create RgbaImage from raw pixels")?;

    let dynamic_image: DynamicImage = DynamicImage::ImageRgba8(rgba_image);
    Ok(dynamic_image.to_luma32f())
}