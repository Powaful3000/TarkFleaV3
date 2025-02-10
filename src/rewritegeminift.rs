// src/rewritegeminift.rs
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
    /// The stride of the captured frame (bytes per row). This accounts for memory alignment padding.
    stride: u32,
    /// The timestamp when the frame was captured.
    timestamp: Instant,
}

// FrameRecorder struct to manage frame capture and buffer
/// `FrameRecorder` manages the capturing and buffering of frames.
/// It uses a singleton pattern to provide global access to the most recent frames, stored in a ring buffer.
pub struct FrameRecorder {
    /// The buffer storing the captured frames. Uses a `VecDeque` for efficient FIFO behavior, acting as a ring buffer.
    buffer: Arc<Mutex<VecDeque<FrameData>>>,
    /// The maximum number of frames to store in the buffer.  Older frames are automatically dropped when new frames arrive and the buffer is full.
    max_frames: usize,
    /// A sender for transmitting `FrameData` to the recorder.  This is used by the capture handler to send new frames to the recorder's buffer.
    frame_sender: mpsc::Sender<FrameData>,
}

impl FrameRecorder {
    /// Creates a new `FrameRecorder` with the specified buffer capacity.
    /// This constructor initializes the frame buffer and starts a background task to manage incoming frames.
    ///
    /// # Arguments
    ///
    /// * `max_frames`: The maximum number of frames to store in the buffer.  This determines the size of the ring buffer.
    fn new(max_frames: usize) -> Self {
        let (frame_sender, frame_receiver) = mpsc::channel(32); // Channel for sending frames to the recorder.  A channel with a small buffer is used to avoid blocking the capture thread.
        let buffer: Arc<Mutex<VecDeque<FrameData>>> = Arc::new(Mutex::new(VecDeque::with_capacity(max_frames))); // Initialize the frame buffer as a VecDeque within a Mutex and Arc for thread-safe access.
        let buffer_clone = buffer.clone(); // Clone the Arc for use in the spawned task.

        // Spawn a tokio task to manage the frame buffer.  This task runs in the background and continuously receives frames and updates the buffer.
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
    /// If the buffer is full, the oldest frame is removed (FIFO behavior - ring buffer). This function runs in a dedicated tokio task.
    ///
    /// # Arguments
    ///
    /// * `buffer`: An `Arc<Mutex<VecDeque<FrameData>>>` representing the frame buffer.  This is shared with the `FrameRecorder` instance.
    /// * `frame_receiver`: An `mpsc::Receiver<FrameData>` for receiving new frames.  Frames captured by `CaptureSessionHandler` are sent through this receiver.
    /// * `max_frames`: The maximum number of frames the buffer can hold.  This is used to enforce the ring buffer behavior.
    async fn frame_buffer_task(
        buffer: Arc<Mutex<VecDeque<FrameData>>>,
        mut frame_receiver: mpsc::Receiver<FrameData>,
        max_frames: usize,
    ) {
        while let Some(frame) = frame_receiver.recv().await {
            let mut guard: MutexGuard<'_, VecDeque<FrameData>> = buffer.lock().await; // Acquire a lock on the buffer to ensure exclusive access.
            if guard.len() >= max_frames {
                guard.pop_front(); // Remove oldest frame if buffer is full, maintaining a fixed-size ring buffer.
            }
            guard.push_back(frame); // Add the latest frame to the back of the buffer.
        }
        println!("Frame receiver channel closed, frame buffer task завершено."); // Log when channel is closed (if needed for debugging or shutdown scenarios).
    }


    /// Retrieves the most recently captured frame from the buffer.
    /// This method acquires a lock on the buffer and returns a clone of the latest frame, if available.
    ///
    /// Returns `None` if the buffer is empty, otherwise returns `Some(FrameData)`. The returned `FrameData` is cloned, so it's safe to use outside of the buffer's lock.
    pub async fn get_latest_frame(&self) -> Option<FrameData> {
        let buffer_lock = self.buffer.lock().await; // Acquire a lock on the buffer to access the frames.
        buffer_lock.back().cloned() // Get a clone of the last element (most recent frame) in the buffer. Returns None if the buffer is empty.
    }

    /// Returns a clone of the `mpsc::Sender<FrameData>` that can be used to send new frames to the recorder.
    /// This sender is used by the `CaptureSessionHandler` to push captured frames into the `FrameRecorder`'s buffer.
    pub fn get_frame_sender(&self) -> mpsc::Sender<FrameData> {
        self.frame_sender.clone() // Cloning the sender is cheap and allows multiple parts of the application to send frames.
    }
}

// Lazy static initialization of the global FrameRecorder singleton
lazy_static! {
    /// A global, lazily-initialized `FrameRecorder` instance.
    /// This provides a single point of access to the captured frames from anywhere in the application.
    /// The `FrameRecorder` is initialized with a buffer capacity of 5 frames.
    static ref GLOBAL_RECORDER: FrameRecorder = FrameRecorder::new(5); // Example: buffer of 5 frames, adjust as needed for performance and memory usage.
}

// Function to access the global FrameRecorder instance
/// Returns a reference to the global `FrameRecorder` instance.
/// This function is the primary way to access the singleton `FrameRecorder` and retrieve the latest captured frames.
pub fn get_global_recorder() -> &'static FrameRecorder {
    &GLOBAL_RECORDER // Returns a static reference to the global FrameRecorder.
}


/// `CaptureSessionHandler` implements the `GraphicsCaptureApiHandler` trait to handle captured frames from the Windows Graphics Capture API.
/// It is responsible for receiving frames, extracting pixel data, and sending `FrameData` to the global `FrameRecorder`.
struct CaptureSessionHandler {
    /// Sender for sending captured frames to the `FrameRecorder`.  This sender is obtained from the global `FrameRecorder` singleton.
    frame_tx: mpsc::Sender<FrameData>,
}

impl GraphicsCaptureApiHandler for CaptureSessionHandler {
    type Flags = String; // Flags type used by the GraphicsCaptureApiHandler trait (String in this case, but not actively used).
    type Error = Box<dyn Error + Send + Sync>; // Error type used by the GraphicsCaptureApiHandler trait.

    /// Creates a new `CaptureSessionHandler`.
    /// This method is called by the `windows-capture` crate when a new capture session is started.
    ///
    /// # Arguments
    ///
    /// * `_context`:  Context associated with the capture session.  These flags are passed from the `Settings` when starting the capture session.
    fn new(_context: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        // Get the global recorder's frame sender to send frames to the buffer.  This ensures that the capture handler sends frames to the global recorder.
        let frame_tx = get_global_recorder().get_frame_sender();
        Ok(Self { frame_tx }) // Initialize the CaptureSessionHandler with the frame sender.
    }

    /// Called when a new frame is available from the capture session.  This is the core method where frame processing happens.
    /// This function extracts the raw pixel data from the `Frame`, creates a `FrameData` object,
    /// and sends it to the `FrameRecorder` via the `frame_tx` channel.
    ///
    /// # Arguments
    ///
    /// * `frame`: A mutable reference to the captured `Frame`.  This contains the raw pixel data and metadata of the captured frame.
    /// * `_capture_control`: An `InternalCaptureControl` object (not used in this implementation).  This can be used to control the capture session, but is ignored here.
    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width(); // Get the width of the captured frame.
        let height = frame.height(); // Get the height of the captured frame.
        let stride = width * 4; // Get the stride of the captured frame.  This is crucial for handling memory alignment correctly.

        if let Ok(mut frame_buffer) = frame.buffer() { // Try to get access to the frame buffer.
            let mut pixels = frame_buffer.as_raw_buffer().to_vec();

            let frame_data = FrameData {
                pixels: Arc::new(pixels), // Wrap the pixel data in an Arc for shared ownership and thread-safety.
                width,
                height,
                stride,
                timestamp: Instant::now(), // Record the capture timestamp.
            };

            // Send the frame data to the recorder.  If the channel is full, this will block until space is available or the sender is dropped.
            // Using blocking_send here ensures that frames are not dropped if the recorder is busy, but it might introduce backpressure on the capture thread.
            if let Err(e) = self.frame_tx.blocking_send(frame_data) {
                eprintln!("Error sending frame to recorder channel: {}", e);
                // Consider handling backpressure or implementing a non-blocking send with a different error handling strategy if needed.
            }

        } else {
            println!("Failed to get frame buffer"); // Log an error if frame buffer access fails.  This might indicate an issue with the capture session or the frame itself.
        }
        Ok(()) // Indicate successful frame processing.
    }

    /// Called when the capture session is closed, either by the user or due to an error.
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended"); // Log session closure.  This can be used to signal the end of the capture process.
        Ok(()) // Indicate successful handling of session closure.
    }
}


/// Main function for the example application.  This function sets up the capture session, loads a template image,
/// and enters a main loop to continuously capture frames, perform template matching, and detect captcha.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Starting Escape From Tarkov Bot POC Rewrite...");

    // Find the target window by its name.  This assumes the game window is named "EscapeFromTarkov".
    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?; // Error handling if the window is not found.
    println!("Found target window: {:?}", target_window);

    // Create capture settings.  These settings configure how the capture session will behave.
    let settings = Settings::new(
        target_window, // Capture the found target window.
        CursorCaptureSettings::Default, // Use default cursor capture settings (show cursor if present).
        DrawBorderSettings::Default, // Use default border drawing settings (draw window border).
        ColorFormat::Rgba8, // Capture in RGBA8 format for compatibility with image processing.
        String::new(), // Flags can be empty string as no special flags are needed.
    );

    println!("Starting capture session...");
    // Start the capture session in a blocking task.  `windows-capture` API requires starting the capture session in a thread that can block.
    let capture_result = tokio::task::spawn_blocking(move || {
        CaptureSessionHandler::start(settings) // Start the capture session with the configured settings and handler.
    }).await;

    // Check the result of starting the capture session.  Handle potential errors during session startup.
    match capture_result {
        Ok(Ok(_)) => println!("Capture session started successfully in background task."), // Log success if the session starts without errors.
        Ok(Err(e)) => eprintln!("Error starting capture session: {}", e), // Log any error that occurs during session startup.
        Err(e) => eprintln!("Error joining capture task: {}", e), // Log errors if the tokio task itself fails to spawn or join.
    }


    // Load template image for template matching.  The template image "captcha-template.png" is expected to be in the same directory as the executable.
    let template_dyn = image::load_from_memory(include_bytes!("captcha-template.png")) // Load the template image from embedded bytes.
        .unwrap() // Panic if the template image fails to load.  Error handling should be more robust in a production application.
        .to_luma32f(); // Convert the template image to grayscale 32-bit float format, suitable for template matching.
    let template_data = template_dyn.as_raw().to_vec(); // Get the raw pixel data of the template image.
    let template_width = template_dyn.width(); // Get the width of the template image.
    let template_height = template_dyn.height(); // Get the height of the template image.
    println!("Template loaded: {}x{}", template_width, template_height); // Log the dimensions of the loaded template.


    let mut matcher = TemplateMatcher::new(); // Create a new TemplateMatcher instance.  This will be used to perform template matching operations.

    println!("Entering main loop to process frames...");
    let mut frame_interval = interval(Duration::from_millis(1000/60)); // Target 60 FPS, though actual FPS depends on capture speed and processing.  Create a tokio interval to control the frame processing rate.

    loop {
        frame_interval.tick().await; // Wait for the next tick of the interval to control frame processing rate.  This helps to limit CPU usage and synchronize processing with the desired frame rate.

        if let Some(frame_data) = get_global_recorder().get_latest_frame().await { // Get the latest frame from the global FrameRecorder.
            println!(
                "Got frame: {}x{}, stride: {}, {} bytes",
                frame_data.width,
                frame_data.height,
                frame_data.stride,
                frame_data.pixels.len()
            );

            // Save frame for debugging.  This is conditionally enabled using a boolean flag.
            if false { // Conditional frame saving, set to true to enable for debug.  Saving frames to disk can be helpful for debugging template matching and capture issues.
                save_frame_to_png(&frame_data, "latest_frame.png").await?; // Save the latest frame to a PNG file.
            }


            // Convert captured frame to grayscale and prepare for template matching.  Template matching is typically performed on grayscale images for efficiency and robustness.
            let frame_luma_image = convert_frame_to_luma32f(&frame_data)?; // Convert the RGBA frame data to grayscale 32-bit float format.
            let frame_image = TemplateImage::new(
                frame_luma_image.as_raw().to_vec(), // Create a TemplateImage from the grayscale pixel data.
                frame_luma_image.width(),
                frame_luma_image.height(),
            );
            let template_image = TemplateImage::new(
                template_data.clone(), // Use the loaded template data.
                template_width,
                template_height,
            );

            // Perform template matching.  Use SumOfSquaredDifferences method for template matching.
            matcher.match_template(
                frame_image,
                template_image,
                MatchTemplateMethod::SumOfSquaredDifferences, // Choose the SumOfSquaredDifferences matching method.  Other methods can be explored for different scenarios.
            );
            let result = matcher.wait_for_result().unwrap(); // Wait for the template matching result to be available.
            let extremes = find_extremes(&result); // Find the minimum and maximum values in the result matrix, which indicate potential template matches.
            println!(
                "Template Match Result: Min Value: {}, Location: {:?}",
                extremes.min_value, extremes.min_value_location // Log the minimum match value and its location.  Lower values in SSD indicate better matches.
            );

            let captcha_threshold = 25.0; // Example threshold.  This threshold needs to be tuned based on testing and the specific template and game conditions.
            if extremes.min_value < captcha_threshold { // Check if the minimum match value is below the threshold, indicating a potential captcha match.
                println!("Captcha match found!! (Value: {})", extremes.min_value); // Log captcha match detection.
            } else {
                println!("Captcha not matched (Value: {})", extremes.min_value); // Log when captcha is not detected based on the threshold.
            }


        } else {
            println!("No frame available yet from recorder."); // Log if no frame is available from the recorder, which might happen at startup or if capture is interrupted.
        }
    }
}


/// Saves a `FrameData` object to a PNG file.  This function is used for debugging and visualizing captured frames.
///
/// # Arguments
///
/// * `frame_data`: A reference to the `FrameData` to save.  This contains the pixel data and metadata of the frame.
/// * `filename`: The name of the file to save the image to.  The file will be saved in the current working directory.
async fn save_frame_to_png(frame_data: &FrameData, filename: &str) -> Result<(), Box<dyn Error>> {
    let file = File::create(filename)?; // Create a file with the given filename.
    let w = BufWriter::new(file); // Create a buffered writer for efficient file writing.
    let mut encoder = png::Encoder::new(w, frame_data.width, frame_data.height); // Create a PNG encoder.
    encoder.set_color(png::ColorType::Rgba); // Set the color type to RGBA, matching the captured frame format.
    encoder.set_depth(png::BitDepth::Eight); // Set the bit depth to 8 bits per channel.
    let mut writer = encoder.write_header()?; // Write the PNG header.
    writer.write_image_data(frame_data.pixels.as_ref())?; // Write the image data to the PNG file.
    println!("Saved frame to {}", filename); // Log successful frame saving.
    Ok(()) // Indicate successful PNG saving.
}


/// Converts a `FrameData` object to a `Luma32F` image buffer.  This converts the RGBA frame data to grayscale 32-bit float format, which is used for template matching.
///
/// # Arguments
///
/// * `frame_data`: A reference to the `FrameData` to convert.  This contains the RGBA pixel data of the frame.
fn convert_frame_to_luma32f(frame_data: &FrameData) -> Result<ImageBuffer<Luma<f32>, Vec<f32>>, Box<dyn Error>> {
    let rgba_image = ImageBuffer::<image::Rgba<u8>, _>::from_raw( // Create an RgbaImage from the raw pixel data.
        frame_data.width,
        frame_data.height,
        frame_data.pixels.as_ref().to_vec(), // Provide the pixel data as a vector of bytes.
    ).ok_or("Failed to create RgbaImage from raw pixels")?; // Handle the case where creating the RgbaImage fails (e.g., invalid dimensions or data).

    let dynamic_image: DynamicImage = DynamicImage::ImageRgba8(rgba_image); // Convert the RgbaImage to a DynamicImage.
    Ok(dynamic_image.to_luma32f()) // Convert the DynamicImage to Luma32F format and return it.
}