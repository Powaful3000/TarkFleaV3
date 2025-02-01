use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use std::time::Instant;
use template_matching::{find_extremes, MatchTemplateMethod, TemplateMatcher};
use tokio::sync::Mutex;
use windows_capture::{
    capture::GraphicsCaptureApiHandler,
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
    window::Window,
};
use image::{DynamicImage, GenericImageView, ImageBuffer, Luma};
use template_matching::Image;

#[derive(Clone)]
struct FrameData {
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    stride: u32,
    timestamp: Instant,
}

struct FrameRecorder {
    buffer: Arc<Mutex<VecDeque<FrameData>>>,
    max_frames: usize,
    tx: tokio::sync::mpsc::Sender<FrameData>,
}

impl FrameRecorder {
    pub fn new(max_frames: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(max_frames)));

        let buffer_clone = buffer.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let mut guard = buffer_clone.lock().await;
                if guard.len() >= max_frames {
                    guard.pop_front();
                }
                guard.push_back(frame);
            }
        });

        Self {
            buffer,
            max_frames,
            tx,
        }
    }

    pub async fn get_latest(&self) -> Option<FrameData> {
        let guard = self.buffer.lock().await;
        println!("Buffer size: {}", guard.len());
        guard.back().cloned()
    }

    pub fn get_sender(&self) -> tokio::sync::mpsc::Sender<FrameData> {
        self.tx.clone()
    }
}

struct CaptureHandler {
    tx: tokio::sync::mpsc::Sender<FrameData>,
    start_time: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = String;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(_: windows_capture::capture::Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            start_time: Instant::now(),
            tx: GLOBAL_SENDER.lock().unwrap().clone(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        // Calculate stride with proper alignment (Windows typically aligns to 32 bits)
        let stride = ((width * 4 + 3) & !3) as u32; // Align to 4 bytes

        // Get the raw pixel data from the frame
        if let Ok(mut frame_buffer) = frame.buffer() {
            let buffer_size = (height * stride) as usize;
            let pixels = frame_buffer
                .as_nopadding_buffer()
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .to_vec();

            // Create frame data with the actual stride
            let frame_data = FrameData {
                pixels: Arc::new(pixels),
                width,
                height,
                stride,
                timestamp: Instant::now(),
            };

            self.tx
                .blocking_send(frame_data)
                .unwrap_or_else(|e| println!("Failed to send frame data: {}", e));
        } else {
            println!("Failed to get frame buffer");
        }

        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Capture session ended");
        Ok(())
    }
}

// Global sender for the capture handler
lazy_static::lazy_static! {
    static ref GLOBAL_SENDER: std::sync::Mutex<tokio::sync::mpsc::Sender<FrameData>> = {
        let (tx, _) = tokio::sync::mpsc::channel(32);
        std::sync::Mutex::new(tx)
    };
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting capture...");
    let recorder = FrameRecorder::new(4); // Keep 60 frames in the buffer

    // Store the sender globally for the capture handler
    *GLOBAL_SENDER.lock().unwrap() = recorder.get_sender();

    let target_window = Window::from_name("EscapeFromTarkov")
        .map_err(|_| "Failed to find EscapeFromTarkov window")?;
    println!("Found target window");

    let settings = Settings::new(
        target_window,
        CursorCaptureSettings::Default,
        DrawBorderSettings::Default,
        ColorFormat::Rgba8,
        String::new(),
    );

    // Start capture in a blocking thread
    println!("Starting capture thread...");
    let capture_handle = tokio::task::spawn_blocking(move || {
        println!("Capture thread started");
        CaptureHandler::start(settings)
    });

    // Create template matcher
    let mut matcher = TemplateMatcher::new();
    let template_dyn = image::load_from_memory(include_bytes!("captcha-template.png"))
        .unwrap()
        .to_luma32f();
    let template_data = template_dyn.as_raw().to_vec();
    let template_width = template_dyn.width();
    let template_height = template_dyn.height();

    // Example: Get the latest frame every second
    println!("Entering main loop...");

    loop {
        if let Some(frame_data) = recorder.get_latest().await {
            println!(
                "Got frame: {}x{}, {} bytes",
                frame_data.width,
                frame_data.height,
                frame_data.pixels.len()
            );
            // Save frame as PNG with correct pixel data access
            let filename = "frame.png";
            let file = File::create(&filename).unwrap();
            let w = BufWriter::new(file);
            let mut encoder = png::Encoder::new(w, frame_data.width, frame_data.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(frame_data.pixels.as_ref()).unwrap();
            println!("Saved frame to {} with {} bytes", filename, frame_data.pixels.len());

            // Create RGBA image from raw pixels with proper data access
            let rgba_image = image::RgbaImage::from_raw(
                frame_data.width,
                frame_data.height,
                frame_data.pixels.as_ref().to_vec()
            ).expect("Invalid frame buffer dimensions");

            // Convert to grayscale then to 32-bit float format
            let frame_luma = DynamicImage::ImageRgba8(rgba_image).to_luma32f();
            
            // Create template matching images and save debug images
            let frame_data_vec = frame_luma.as_raw().to_vec();
            
            // Save frame_image as PNG first
            let frame_image_filename = "frame_image.png";
            let frame_image_file = File::create(&frame_image_filename).unwrap();
            let frame_image_w = BufWriter::new(frame_image_file);
            let mut frame_image_encoder = png::Encoder::new(frame_image_w, frame_luma.width(), frame_luma.height());
            frame_image_encoder.set_color(png::ColorType::Grayscale);
            frame_image_encoder.set_depth(png::BitDepth::Eight);
            let mut frame_image_writer = frame_image_encoder.write_header().unwrap();
            // Convert f32 data to u8
            let frame_u8: Vec<u8> = frame_data_vec.iter()
                .map(|&x| (x * 255.0).clamp(0.0, 255.0) as u8)
                .collect();
            frame_image_writer.write_image_data(&frame_u8).unwrap();
            println!("Saved frame_image to {}", frame_image_filename);

            // Save template_image as PNG
            let template_image_filename = "template_image.png";
            let template_image_file = File::create(&template_image_filename).unwrap();
            let template_image_w = BufWriter::new(template_image_file);
            let mut template_image_encoder = png::Encoder::new(template_image_w, template_width, template_height);
            template_image_encoder.set_color(png::ColorType::Grayscale);
            template_image_encoder.set_depth(png::BitDepth::Eight);
            let mut template_image_writer = template_image_encoder.write_header().unwrap();
            // Convert f32 data to u8
            let template_u8: Vec<u8> = template_data.iter()
                .map(|&x| (x * 255.0).clamp(0.0, 255.0) as u8)
                .collect();
            template_image_writer.write_image_data(&template_u8).unwrap();
            println!("Saved template_image to {}", template_image_filename);

            // Now create the images for template matching
            let frame_image = Image::new(
                frame_data_vec,
                frame_luma.width(),
                frame_luma.height(),
            );
            let template_image = Image::new(
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
            // correct match ~~ 6
            // main menu ~~ 60
            let result = matcher.wait_for_result().unwrap();
            let extremes = find_extremes(&result);
            println!(
                "Captcha Match found at {:?} with confidence: {}",
                extremes.min_value_location, extremes.min_value
            );
            let captcha_threshold = 25.0;
            if extremes.min_value < captcha_threshold {
                println!("Captcha match found!! {}% within threshold", (extremes.min_value / captcha_threshold) * 100.0);
            }
        } else {
            println!("No frame available");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}
